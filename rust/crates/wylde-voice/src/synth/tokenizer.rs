//! Phoneme-string → Kokoro token-ID sequence.
//!
//! Mirrors the deterministic half of `kokoro_onnx.tokenizer.Tokenizer`:
//! the `tokenize` step (vocab lookup + pad-with-zero) is bit-exact with
//! the Python implementation because both consume the same vocab table
//! from `kokoro_onnx/config.json`. The non-deterministic upstream step
//! — text → phonemes via espeak-ng — is deliberately **not** in this
//! module's scope; see [`crate::synth::tokenizer`] doc below.
//!
//! ## Why phonemes-only for Slice 11.B
//!
//! The Python pipeline phonemises text with espeak-ng (via the
//! `phonemizer` Python lib + `espeakng_loader`). Reproducing that in
//! Rust requires either (a) FFI to libespeak-ng or (b) shelling out to
//! the espeak-ng CLI — both add a non-trivial native dep that the
//! strangler-fig doesn't strictly need yet. While `WYLDE_WYLDE_VOICE_IMPL`
//! defaults to `python`, the Python orchestrator owns the text-side
//! phonemisation; Rust just has to do the deterministic ONNX-side work.
//!
//! Slice 11.B+ (text-path phonemisation) is the natural follow-up: either
//! wire the `espeak-ng` crate or call out to a tiny Python helper. For
//! parity testing, callers pass the espeak-produced phoneme string
//! directly via `payload.phonemes`.

use crate::synth::vocab;

/// Result of tokenising a phoneme string. `tokens` is the un-padded
/// inner sequence (the caller will sandwich it with `0` pads before
/// the model call). `truncated` is `true` when the input exceeded
/// [`vocab::MAX_PHONEME_LENGTH`] — same warn-and-clip semantics as
/// `kokoro_onnx._create_audio`.
#[derive(Debug, Clone)]
pub struct TokenizeResult {
    pub tokens: Vec<i64>,
    pub truncated: bool,
}

/// Tokenise a phoneme string into the Kokoro token-id sequence.
/// Out-of-vocab code-points are dropped (Python:
/// `filter(lambda p: p in vocab, phonemes)`). Sequences longer than
/// the model's context are truncated to [`vocab::MAX_PHONEME_LENGTH`].
pub fn tokenize(phonemes: &str) -> TokenizeResult {
    let mut tokens: Vec<i64> = Vec::with_capacity(phonemes.len().min(vocab::MAX_PHONEME_LENGTH));
    let mut overflowed = false;
    for ch in phonemes.chars() {
        let Some(id) = vocab::lookup(ch) else { continue };
        if tokens.len() == vocab::MAX_PHONEME_LENGTH {
            overflowed = true;
            break;
        }
        tokens.push(i64::from(id));
    }
    TokenizeResult {
        tokens,
        truncated: overflowed,
    }
}

/// Wrap the tokeniser output in the `[0, ..., 0]` pad the Kokoro ONNX
/// model expects on its `input_ids`. Output length is `tokens.len() + 2`.
pub fn pad_with_zero(tokens: &[i64]) -> Vec<i64> {
    let mut padded = Vec::with_capacity(tokens.len() + 2);
    padded.push(0);
    padded.extend_from_slice(tokens);
    padded.push(0);
    padded
}

/// Split a phoneme string into sentence-shaped sub-strings for the
/// streaming TTS path (`voice.synthesize_stream`). Splits on terminal
/// IPA-adjacent punctuation (`.`, `!`, `?`, `;`) and newlines — the same
/// punctuation rows the Kokoro tokenizer treats as natural prosody
/// boundaries. Trailing whitespace is dropped, but the punctuation
/// itself is kept on the chunk that produced it (matches the Python
/// `kokoro_onnx` heuristic of "keep the pause cue with its sentence").
///
/// Each returned slice is a borrow into `phonemes`. Empty / whitespace-
/// only fragments are skipped, so caller never sees a zero-length chunk.
/// If the input has no terminator at all, the whole string is returned
/// as a single chunk — never panics, never returns empty for non-empty
/// input.
pub fn split_phonemes(phonemes: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0_usize;
    for (idx, ch) in phonemes.char_indices() {
        if matches!(ch, '.' | '!' | '?' | ';' | '\n') {
            let end = idx + ch.len_utf8();
            let piece = phonemes[start..end].trim();
            if !piece.is_empty() {
                let piece_start = start + phonemes[start..end].find(piece).unwrap_or(0);
                out.push(&phonemes[piece_start..piece_start + piece.len()]);
            }
            start = end;
        }
    }
    if start < phonemes.len() {
        let tail = phonemes[start..].trim();
        if !tail.is_empty() {
            let tail_start = start + phonemes[start..].find(tail).unwrap_or(0);
            out.push(&phonemes[tail_start..tail_start + tail.len()]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_known_phoneme_string() {
        // espeak-ng output for "hello" with en-us, with_stress=True,
        // preserve_punctuation=True is the IPA "həlˈoʊ". Token IDs from
        // Kokoro's vocab: h=50, ə=83, l=54, ˈ=156, o=57, ʊ=135.
        let result = tokenize("həlˈoʊ");
        assert!(!result.truncated);
        assert_eq!(result.tokens, vec![50, 83, 54, 156, 57, 135]);
    }

    #[test]
    fn out_of_vocab_chars_are_dropped() {
        // 'B' isn't in the upper-case subset (A I O Q S T W Y only),
        // so "Bat" should produce only ['a' → 43, 't' → 62].
        let result = tokenize("Bat");
        assert!(!result.truncated);
        assert_eq!(result.tokens, vec![43, 62]);
    }

    #[test]
    fn truncates_to_max_phoneme_length() {
        // Build a 600-char input that's pure vocab characters; expect
        // it to clip at 510 with truncated=true.
        let input: String = "a".repeat(600);
        let result = tokenize(&input);
        assert!(result.truncated);
        assert_eq!(result.tokens.len(), vocab::MAX_PHONEME_LENGTH);
    }

    #[test]
    fn does_not_truncate_at_exactly_max_length() {
        let input: String = "a".repeat(vocab::MAX_PHONEME_LENGTH);
        let result = tokenize(&input);
        assert!(!result.truncated);
        assert_eq!(result.tokens.len(), vocab::MAX_PHONEME_LENGTH);
    }

    #[test]
    fn pad_with_zero_wraps_the_sequence() {
        let padded = pad_with_zero(&[50, 83, 54]);
        assert_eq!(padded, vec![0, 50, 83, 54, 0]);
    }

    #[test]
    fn pad_with_zero_handles_empty_input() {
        let padded = pad_with_zero(&[]);
        assert_eq!(padded, vec![0, 0]);
    }

    #[test]
    fn empty_string_tokenises_to_empty() {
        let result = tokenize("");
        assert!(!result.truncated);
        assert!(result.tokens.is_empty());
    }

    #[test]
    fn spaces_and_punctuation_kept() {
        // Spaces and recognised punctuation are vocab items, so they
        // SHOULD round-trip.
        let result = tokenize("a. b");
        assert!(!result.truncated);
        // 'a'=43, '.'=4, ' '=16, 'b'=44
        assert_eq!(result.tokens, vec![43, 4, 16, 44]);
    }

    #[test]
    fn split_phonemes_breaks_on_sentence_terminators() {
        let parts = split_phonemes("həlˈoʊ. wˈɜːld! hˈaʊ ɑːɹ jˈuː?");
        assert_eq!(
            parts,
            vec!["həlˈoʊ.", "wˈɜːld!", "hˈaʊ ɑːɹ jˈuː?"],
            "should keep punctuation with the preceding sentence",
        );
    }

    #[test]
    fn split_phonemes_handles_no_terminator() {
        // No `.`/`!`/`?`/`;`/`\n` — returned as a single chunk.
        let parts = split_phonemes("həlˈoʊ wˈɜːld");
        assert_eq!(parts, vec!["həlˈoʊ wˈɜːld"]);
    }

    #[test]
    fn split_phonemes_drops_empty_fragments() {
        // Runs of terminators + whitespace must NOT produce zero-length
        // chunks — the synth ONNX call would reject empty input.
        let parts = split_phonemes("a. . . b.");
        assert_eq!(parts, vec!["a.", ".", ".", "b."]);
    }

    #[test]
    fn split_phonemes_handles_newline_boundary() {
        // Newline counts as a chunk boundary alongside punctuation.
        let parts = split_phonemes("a\nb\nc");
        assert_eq!(parts, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_phonemes_empty_input_returns_empty() {
        // Whitespace-only / empty inputs trim away in every branch.
        // Pure-punctuation runs still yield single-char chunks (Kokoro
        // treats punctuation as prosody cues) — see the empty-fragment
        // test for that shape.
        assert!(split_phonemes("").is_empty());
        assert!(split_phonemes("   ").is_empty());
        assert!(split_phonemes("\n\n").is_empty());
    }

    #[test]
    fn split_phonemes_preserves_unicode_phoneme_chars() {
        // Stress markers + IPA glyphs are multi-byte UTF-8 — ensure we
        // never byte-index inside a char and the returned slices stay
        // valid UTF-8.
        let parts = split_phonemes("ˈeɪ. ˈbiː.");
        assert_eq!(parts, vec!["ˈeɪ.", "ˈbiː."]);
        for p in parts {
            assert!(p.is_char_boundary(0));
            assert!(p.is_char_boundary(p.len()));
        }
    }
}
