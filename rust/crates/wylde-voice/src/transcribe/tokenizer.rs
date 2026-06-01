//! Whisper tokenizer integration.
//!
//! Thin wrapper around the [`tokenizers`] crate (Hugging Face's Rust
//! tokenizer library — same code Python `transformers` calls into via
//! the `tokenizers` Python binding). Loads the `tokenizer.json` shipped
//! in every onnx-community Whisper snapshot — BPE merges round-trip
//! bit-exact with the Python pipeline because both sides use the same
//! library.
//!
//! Two consumer surfaces:
//!
//! * [`WhisperTokenizer::build_prompt`] — emit the seeded `input_ids`
//!   prefix for the decoder. For English-only models (`whisper-tiny.en`,
//!   `whisper-small.en`, etc.) that's just `<|startoftranscript|>
//!   <|notimestamps|>` (token IDs 50257, 50362). For multilingual models
//!   the language and `<|transcribe|>` tokens go between them.
//! * [`WhisperTokenizer::decode`] — convert a token-ID sequence back to
//!   text, stripping the prompt prefix + any trailing special tokens.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokenizers::Tokenizer;

/// English-only Whisper start tokens. Matches the `forced_decoder_ids`
/// in `generation_config.json` for `openai/whisper-tiny.en`:
/// `[(1, 50362)]` — position 1 is forced to `<|notimestamps|>`.
const SOT_TOKEN: &str = "<|startoftranscript|>";
const NOTIMESTAMPS_TOKEN: &str = "<|notimestamps|>";
const TRANSCRIBE_TOKEN: &str = "<|transcribe|>";
const EOT_TOKEN: &str = "<|endoftext|>";

#[derive(Debug, Error)]
pub enum TokenizerLoadError {
    #[error("tokenizer.json not found at {0}")]
    NotFound(PathBuf),

    #[error("tokenizer load failed: {0}")]
    Load(String),

    #[error("missing special token in tokenizer vocab: {0}")]
    MissingSpecialToken(String),
}

#[derive(Debug, Error)]
pub enum TokenizerDecodeError {
    #[error("decode failed: {0}")]
    Decode(String),
}

/// Loaded Whisper tokenizer plus the resolved IDs of the special tokens
/// that drive the decoder prompt / stop condition. Cheap to clone for
/// borrow-friendly call sites — the heavy state is in the underlying
/// `Tokenizer` which is `Arc`-wrapped internally.
pub struct WhisperTokenizer {
    tokenizer: Tokenizer,
    sot_id: i64,
    notimestamps_id: i64,
    transcribe_id: i64,
    eot_id: i64,
    is_multilingual: bool,
}

impl WhisperTokenizer {
    /// Load from a `tokenizer.json` path — typically the file in the
    /// HF snapshot directory next to `config.json`.
    ///
    /// `is_multilingual` is canonically derived from the model's
    /// `config.json` (the `forced_decoder_ids` length tells you: 1
    /// element for English-only `*.en` models, 3 elements for
    /// multilingual). Whisper's tokenizer.json ships ALL language
    /// special tokens regardless of model variant, so the tokenizer
    /// alone can't tell you which prompt shape to emit. The caller
    /// reads config.json once and passes the flag here.
    pub fn load(tokenizer_json: &Path, is_multilingual: bool) -> Result<Self, TokenizerLoadError> {
        if !tokenizer_json.exists() {
            return Err(TokenizerLoadError::NotFound(tokenizer_json.to_path_buf()));
        }
        let tokenizer = Tokenizer::from_file(tokenizer_json)
            .map_err(|e| TokenizerLoadError::Load(e.to_string()))?;

        let sot_id = require_special_id(&tokenizer, SOT_TOKEN)?;
        let notimestamps_id = require_special_id(&tokenizer, NOTIMESTAMPS_TOKEN)?;
        let eot_id = require_special_id(&tokenizer, EOT_TOKEN)?;
        let transcribe_id = tokenizer
            .token_to_id(TRANSCRIBE_TOKEN)
            .map(|id| id as i64)
            .unwrap_or(-1);

        Ok(Self {
            tokenizer,
            sot_id,
            notimestamps_id,
            transcribe_id,
            eot_id,
            is_multilingual,
        })
    }

    /// Build the seeded decoder prompt for `language` (e.g. `"en"`,
    /// `"fr"`). For English-only models the `language` argument is
    /// ignored; the prompt is just `[SOT, NOTIMESTAMPS]`. For
    /// multilingual models it expands to `[SOT, <|{lang}|>, TRANSCRIBE,
    /// NOTIMESTAMPS]`. Returns an error if the requested language token
    /// is not in the vocab.
    pub fn build_prompt(&self, language: &str) -> Result<Vec<i64>, TokenizerLoadError> {
        if !self.is_multilingual {
            return Ok(vec![self.sot_id, self.notimestamps_id]);
        }
        if self.transcribe_id < 0 {
            return Err(TokenizerLoadError::MissingSpecialToken(
                TRANSCRIBE_TOKEN.to_owned(),
            ));
        }
        let lang_token = format!("<|{}|>", language);
        let lang_id = self
            .tokenizer
            .token_to_id(&lang_token)
            .ok_or(TokenizerLoadError::MissingSpecialToken(lang_token))?;
        Ok(vec![
            self.sot_id,
            lang_id as i64,
            self.transcribe_id,
            self.notimestamps_id,
        ])
    }

    /// Decode a token-ID sequence back to text. Skips special tokens
    /// (`<|...|>` markers) so the caller can hand back the raw
    /// transcript.
    pub fn decode(&self, ids: &[i64]) -> Result<String, TokenizerDecodeError> {
        let u32_ids: Vec<u32> = ids.iter().filter_map(|&i| u32::try_from(i).ok()).collect();
        self.tokenizer
            .decode(&u32_ids, true)
            .map_err(|e| TokenizerDecodeError::Decode(e.to_string()))
    }

    pub fn eot_id(&self) -> i64 {
        self.eot_id
    }

    pub fn sot_id(&self) -> i64 {
        self.sot_id
    }

    pub fn is_multilingual(&self) -> bool {
        self.is_multilingual
    }
}

fn require_special_id(t: &Tokenizer, token: &str) -> Result<i64, TokenizerLoadError> {
    t.token_to_id(token)
        .map(|id| id as i64)
        .ok_or_else(|| TokenizerLoadError::MissingSpecialToken(token.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the bundled whisper-tiny.en tokenizer.json under the HF
    /// cache. Tests that need the real tokenizer guard on its presence
    /// and skip otherwise — this keeps `cargo test -p wylde-voice`
    /// green on CI machines that haven't downloaded the model.
    fn tokenizer_path() -> Option<PathBuf> {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
        let p = PathBuf::from(home)
            .join(".cache/huggingface/hub")
            .join("models--onnx-community--whisper-tiny.en")
            .join("snapshots")
            .join("2575352d61be1bf7225cf8f8b268a4678025fc58")
            .join("tokenizer.json");
        if p.exists() {
            Some(p)
        } else {
            None
        }
    }

    #[test]
    fn load_missing_file_returns_not_found() {
        let r = WhisperTokenizer::load(Path::new("/no/such/tokenizer.json"), false);
        assert!(matches!(r, Err(TokenizerLoadError::NotFound(_))));
    }

    #[test]
    fn english_only_prompt_has_two_tokens() {
        let Some(path) = tokenizer_path() else {
            eprintln!("skipping: whisper-tiny.en tokenizer not cached");
            return;
        };
        let t = WhisperTokenizer::load(&path, false).unwrap();
        assert!(!t.is_multilingual(), "whisper-tiny.en is English-only");
        let prompt = t.build_prompt("en").unwrap();
        assert_eq!(prompt, vec![50257_i64, 50362_i64]);
        assert_eq!(t.eot_id(), 50256);
    }

    #[test]
    fn multilingual_prompt_inserts_language_and_transcribe_tokens() {
        let Some(path) = tokenizer_path() else {
            eprintln!("skipping: whisper-tiny.en tokenizer not cached");
            return;
        };
        // Force multilingual prompt construction even on the .en
        // tokenizer (which carries the lang tokens anyway). Validates
        // the 4-token prompt shape.
        let t = WhisperTokenizer::load(&path, true).unwrap();
        let prompt = t.build_prompt("fr").unwrap();
        assert_eq!(
            prompt,
            vec![50257_i64, 50264_i64, 50358_i64, 50362_i64],
            "expected [SOT, <|fr|>, <|transcribe|>, <|notimestamps|>]"
        );
    }

    #[test]
    fn multilingual_prompt_rejects_unknown_language() {
        let Some(path) = tokenizer_path() else {
            return;
        };
        let t = WhisperTokenizer::load(&path, true).unwrap();
        let err = t.build_prompt("klingon").unwrap_err();
        assert!(matches!(err, TokenizerLoadError::MissingSpecialToken(_)));
    }

    #[test]
    fn decode_skips_special_tokens() {
        let Some(path) = tokenizer_path() else {
            eprintln!("skipping: whisper-tiny.en tokenizer not cached");
            return;
        };
        let t = WhisperTokenizer::load(&path, false).unwrap();
        // Mix of special + regular tokens — special ones must be dropped.
        // 50256 = <|endoftext|>, 50257 = <|startoftranscript|>.
        // " hello" decodes to a leading-space "hello" in Whisper BPE.
        let hello = t.tokenizer.encode(" hello", false).unwrap();
        let mut ids: Vec<i64> = vec![50257];
        for u in hello.get_ids() {
            ids.push(*u as i64);
        }
        ids.push(50256);
        let text = t.decode(&ids).unwrap();
        assert!(
            text.to_lowercase().contains("hello"),
            "decoded should contain 'hello', got {text:?}"
        );
        assert!(
            !text.contains("<|"),
            "special-token markers leaked into decoded text: {text:?}"
        );
    }
}
