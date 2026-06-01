//! Kokoro phoneme vocabulary — char → token id.
//!
//! Lifted verbatim from `kokoro_onnx/config.json::vocab`, the static
//! mapping every Kokoro ONNX export ships with. 116 entries; sparse
//! integer codes up to 177 because Kokoro's training set evolved over
//! the original 178-token alphabet and the upstream config skips
//! reserved IDs. Tokenizer hot-path is a single `match` so this stays
//! free of any HashMap allocation overhead.
//!
//! Anything outside this table (e.g. ASCII letters with no phoneme
//! interpretation, or IPA glyphs not in the Kokoro vocab) is dropped
//! upstream by [`crate::synth::tokenizer`] — the Python pipeline does
//! the same `filter(lambda p: p in vocab, phonemes)` step in
//! [`kokoro_onnx.tokenizer.Tokenizer.phonemize`].

/// Resolve a single phoneme `char` (well, code-point) to its Kokoro
/// token id, or `None` if it is not in the vocabulary. Caller drops
/// `None`s to match Python `filter(lambda p: p in self.vocab, phonemes)`.
pub fn lookup(ch: char) -> Option<u32> {
    Some(match ch {
        ';' => 1,
        ':' => 2,
        ',' => 3,
        '.' => 4,
        '!' => 5,
        '?' => 6,
        '\u{2014}' => 9,    // em-dash —
        '\u{2026}' => 10,   // ellipsis …
        '"' => 11,
        '(' => 12,
        ')' => 13,
        '\u{201C}' => 14,   // “
        '\u{201D}' => 15,   // ”
        ' ' => 16,
        '\u{0303}' => 17,   // combining tilde
        'ʣ' => 18,
        'ʥ' => 19,
        'ʦ' => 20,
        'ʨ' => 21,
        'ᵝ' => 22,
        '\u{AB67}' => 23,   // ꭧ (latin small letter glottal stop with stroke)
        'A' => 24,
        'I' => 25,
        'O' => 31,
        'Q' => 33,
        'S' => 35,
        'T' => 36,
        'W' => 39,
        'Y' => 41,
        'ᵊ' => 42,
        'a' => 43,
        'b' => 44,
        'c' => 45,
        'd' => 46,
        'e' => 47,
        'f' => 48,
        'h' => 50,
        'i' => 51,
        'j' => 52,
        'k' => 53,
        'l' => 54,
        'm' => 55,
        'n' => 56,
        'o' => 57,
        'p' => 58,
        'q' => 59,
        'r' => 60,
        's' => 61,
        't' => 62,
        'u' => 63,
        'v' => 64,
        'w' => 65,
        'x' => 66,
        'y' => 67,
        'z' => 68,
        'ɑ' => 69,
        'ɐ' => 70,
        'ɒ' => 71,
        'æ' => 72,
        'β' => 75,
        'ɔ' => 76,
        'ɕ' => 77,
        'ç' => 78,
        'ɖ' => 80,
        'ð' => 81,
        'ʤ' => 82,
        'ə' => 83,
        'ɚ' => 85,
        'ɛ' => 86,
        'ɜ' => 87,
        'ɟ' => 90,
        'ɡ' => 92,
        'ɥ' => 99,
        'ɨ' => 101,
        'ɪ' => 102,
        'ʝ' => 103,
        'ɯ' => 110,
        'ɰ' => 111,
        'ŋ' => 112,
        'ɳ' => 113,
        'ɲ' => 114,
        'ɴ' => 115,
        'ø' => 116,
        'ɸ' => 118,
        'θ' => 119,
        'œ' => 120,
        'ɹ' => 123,
        'ɾ' => 125,
        'ɻ' => 126,
        'ʁ' => 128,
        'ɽ' => 129,
        'ʂ' => 130,
        'ʃ' => 131,
        'ʈ' => 132,
        'ʧ' => 133,
        'ʊ' => 135,
        'ʋ' => 136,
        'ʌ' => 138,
        'ɣ' => 139,
        'ɤ' => 140,
        'χ' => 142,
        'ʎ' => 143,
        'ʒ' => 147,
        'ʔ' => 148,
        'ˈ' => 156,
        'ˌ' => 157,
        'ː' => 158,
        'ʰ' => 162,
        'ʲ' => 164,
        '↓' => 169,
        '→' => 171,
        '↗' => 172,
        '↘' => 173,
        'ᵻ' => 177,
        _ => return None,
    })
}

/// Max phoneme sequence length Kokoro's encoder accepts. Mirrors
/// `kokoro_onnx.config.MAX_PHONEME_LENGTH`. Sequences longer than this
/// are truncated (same as Python's `phonemes[:MAX_PHONEME_LENGTH]`).
pub const MAX_PHONEME_LENGTH: usize = 510;

/// Kokoro's native sample rate. Mirrors `kokoro_onnx.config.SAMPLE_RATE`.
pub const KOKORO_SAMPLE_RATE: u32 = 24_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_letters_in_vocab_resolve() {
        assert_eq!(lookup('a'), Some(43));
        assert_eq!(lookup('z'), Some(68));
        assert_eq!(lookup('A'), Some(24));
        assert_eq!(lookup('Y'), Some(41));
    }

    #[test]
    fn ipa_phonemes_resolve() {
        // Common English IPA phonemes Kokoro emits.
        assert_eq!(lookup('ə'), Some(83));
        assert_eq!(lookup('ʃ'), Some(131));
        assert_eq!(lookup('θ'), Some(119));
        assert_eq!(lookup('ɹ'), Some(123));
    }

    #[test]
    fn punctuation_resolves() {
        assert_eq!(lookup(' '), Some(16));
        assert_eq!(lookup('.'), Some(4));
        assert_eq!(lookup(','), Some(3));
        assert_eq!(lookup('?'), Some(6));
        assert_eq!(lookup('!'), Some(5));
    }

    #[test]
    fn out_of_vocab_returns_none() {
        // ASCII letters Kokoro doesn't carry (B, C, D, ...) — only A I O
        // Q S T W Y are in the upper-case set.
        assert_eq!(lookup('B'), None);
        assert_eq!(lookup('C'), None);
        // Random emoji.
        assert_eq!(lookup('🦀'), None);
        // Stress tokens that aren't in the vocab.
        assert_eq!(lookup('\u{30A}'), None); // combining ring above
    }

    #[test]
    fn vocab_ids_are_under_178() {
        // Kokoro's `n_token` is 178; every id we emit must fit so the
        // input_ids tensor stays inside the embedding bound.
        let probe_chars: &[char] = &[
            ';', ':', ',', '.', '!', '?', '\u{2014}', ' ', 'A', 'a', 'z',
            'ə', 'ʃ', 'θ', 'ɹ', '↓', 'ᵻ', 'ʰ',
        ];
        for &c in probe_chars {
            let id = lookup(c).expect("probe char in vocab");
            assert!(id < 178, "{c:?} → {id} exceeds n_token=178");
        }
    }
}
