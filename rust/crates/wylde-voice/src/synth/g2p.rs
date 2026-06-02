//! Grapheme-to-phoneme (G2P) — English text → Kokoro IPA phoneme string.
//!
//! ## Slice 2 of the voice Rust port
//!
//! This is the piece [`docs/plans/voice-rust-port.md`](../../../../docs/plans/voice-rust-port.md)
//! flagged as the hardest remaining gap. Until now the `text → phonemes`
//! step ran **Python-side**: the orchestrator's TTS hop sent
//! `models.synthesize {text}` to the Python harness, which called
//! `Voice/synthesize.py` → `kokoro_onnx.Kokoro.create(text)` → espeak-ng
//! (via `phonemizer` + `espeakng_loader`). `wylde-voice`'s own
//! [`crate::actions::synthesize`] only accepted *phonemes*.
//!
//! This module closes that gap in pure Rust, so the whole TTS path
//! (`text → phonemes → tokens → Kokoro ONNX → WAV`) runs inside
//! `wylde-voice` with no Python dependency.
//!
//! ## Why `misaki-rs`
//!
//! Kokoro's *own* reference phonemiser is [misaki](https://github.com/hexgrad/misaki);
//! `misaki-rs` is a self-contained Rust port of it. Its IPA inventory
//! therefore lands squarely in the Kokoro phoneme vocab
//! ([`crate::synth::vocab`]) — the only code-point it emits that our vocab
//! doesn't carry is the zero-width joiner (`U+200D`) inside diphthongs
//! (e.g. `o‍ʊ`, `e‍ɪ`, `d‍ʒ`), which [`crate::synth::tokenize`] drops on the
//! way to token ids exactly like the Python `filter(lambda p: p in vocab)`
//! step. Number-spelling ("72" → "seventy two") and POS-aware heteronym
//! handling ("object" noun vs. verb) come for free.
//!
//! We build with `default-features = false`, dropping `misaki-rs`'s
//! optional `espeak-rs` (C FFI) fallback — that keeps the build pure-Rust
//! (the everything-Rust rule) and avoids shipping a libespeak-ng DLL +
//! data dir. The cost: out-of-vocab words spell out letter-by-letter
//! rather than being guessed by espeak. The embedded misaki lexicons
//! cover common English, so for assistant prose this is rare.
//!
//! ## Dialect selection
//!
//! Kokoro voices carry a dialect in their name prefix (`a*` = American,
//! `b*` = British). We mirror the Python kokoro behaviour of picking the
//! `lang_code` from the voice prefix: a `b…` voice phonemises with
//! en-GB, everything else with en-US. Each dialect's engine is built
//! lazily on first use (each parses a multi-MB embedded lexicon), so a
//! US-only deployment never pays for the GB tables.

use std::sync::OnceLock;

use misaki_rs::language::Language;
use misaki_rs::G2P;

/// Lazily-built en-US engine. `G2P` is `Send + Sync` (its trait-object
/// fields are bounded `Send + Sync`) and `g2p()` takes `&self`, so a
/// single shared instance serves all concurrent synth calls.
fn engine_us() -> &'static G2P {
    static US: OnceLock<G2P> = OnceLock::new();
    US.get_or_init(|| G2P::new(Language::EnglishUS))
}

/// Lazily-built en-GB engine — only initialised the first time a British
/// voice is synthesised.
fn engine_gb() -> &'static G2P {
    static GB: OnceLock<G2P> = OnceLock::new();
    GB.get_or_init(|| G2P::new(Language::EnglishGB))
}

/// Pick the dialect for a Kokoro voice name. `b*` voices (e.g.
/// `bf_emma`, `bm_george`) are British; everything else is American.
pub fn british_for_voice(voice: &str) -> bool {
    voice.starts_with('b') || voice.starts_with('B')
}

/// Convert English `text` into a Kokoro-compatible IPA phoneme string.
///
/// `british` selects the en-GB lexicon (see [`british_for_voice`]).
/// Returns the raw misaki phoneme string; the caller feeds it straight
/// into [`crate::synth::tokenize`], which drops any code-point outside the
/// Kokoro vocab (notably the diphthong zero-width joiners misaki emits).
///
/// On the (practically unreachable) misaki error path this returns an
/// empty string; [`crate::actions::synthesize`] then surfaces the usual
/// "nothing to synthesize" `invalid_request` rather than panicking.
pub fn text_to_phonemes(text: &str, british: bool) -> String {
    let engine = if british { engine_gb() } else { engine_us() };
    match engine.g2p(text) {
        Ok((phonemes, _tokens)) => phonemes,
        Err(e) => {
            tracing::warn!("wylde-voice: G2P failed for {text:?}: {e}");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::tokenize;

    #[test]
    fn dialect_picked_from_voice_prefix() {
        assert!(british_for_voice("bf_emma"));
        assert!(british_for_voice("bm_george"));
        assert!(!british_for_voice("af_heart"));
        assert!(!british_for_voice("am_adam"));
    }

    #[test]
    fn hello_world_phonemises_to_known_ipa() {
        let ph = text_to_phonemes("Hello world", false);
        assert!(!ph.is_empty(), "G2P returned nothing");
        // misaki emits the canonical "həlˈoʊ ... wˈɜːld" shape — assert
        // the load-bearing IPA glyphs are present so a lexicon regression
        // (e.g. letter-spelling fallback) is caught.
        assert!(ph.contains('ə'), "expected schwa in {ph:?}");
        assert!(ph.contains('ˈ'), "expected primary stress in {ph:?}");
        assert!(ph.contains('ɜ') || ph.contains('ɝ'), "expected NURSE vowel in {ph:?}");
    }

    #[test]
    fn phonemes_are_all_in_kokoro_vocab() {
        // Every glyph misaki emits (bar the diphthong zero-width joiner)
        // must resolve in the Kokoro vocab, otherwise the tokenizer would
        // silently drop phonetic content and degrade the audio.
        let ph = text_to_phonemes(
            "The quick brown fox jumps over the lazy dog. I have 7 messages.",
            false,
        );
        let dropped: Vec<char> = ph
            .chars()
            .filter(|c| *c != '\u{200d}') // diphthong ZWJ — intentionally dropped
            .filter(|c| crate::synth::vocab::lookup(*c).is_none())
            .collect();
        assert!(
            dropped.is_empty(),
            "G2P emitted glyphs outside the Kokoro vocab: {dropped:?} (full: {ph:?})",
        );
    }

    #[test]
    fn numbers_are_spelled_out() {
        // misaki's num2words pass should turn digits into words, not drop
        // them — "7" must yield the phonemes for "seven", which tokenise
        // to a non-empty sequence.
        let ph = text_to_phonemes("7", false);
        let result = tokenize(&ph);
        assert!(
            !result.tokens.is_empty(),
            "number did not phonemise to any tokens: {ph:?}",
        );
    }

    #[test]
    fn text_round_trips_into_nonempty_tokens() {
        // The end-to-end contract the synthesize action depends on: plain
        // text → phonemes → at least one in-vocab Kokoro token.
        let ph = text_to_phonemes("Sure, I can help with that.", false);
        let result = tokenize(&ph);
        assert!(!result.tokens.is_empty(), "no tokens from {ph:?}");
        assert!(!result.truncated);
    }
}
