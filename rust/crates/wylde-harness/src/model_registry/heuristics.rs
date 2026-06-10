//! Repo-name → kind inference fallback. Rust port of
//! `Core/harness/model_registry/_heuristics.py`.
//!
//! Service manifests are the source of truth; anything in the HF cache
//! that no manifest claims falls through here. Patterns are intentionally
//! narrow — a wrong guess is better than mis-routing a chat model into
//! the voice subsystem, so when nothing matches we default to `Llm`.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::model_registry::types::Kind;

/// Order matters: the first pattern that matches wins. Place the
/// discriminative tokens (whisper, piper, florence) ahead of broader
/// ones. Mirrors `_PATTERNS` in `_heuristics.py` exactly.
const PATTERNS: &[(&str, Kind)] = &[
    // speech-to-text
    (r"whisper", Kind::Stt),
    (r"\bwav2vec", Kind::Stt),
    (r"\bdistil[-_]?whisper", Kind::Stt),
    // text-to-speech
    (r"\bpiper\b", Kind::Tts),
    (r"\bkokoro\b", Kind::Tts),
    (r"\bxtts\b", Kind::Tts),
    (r"\bbark\b", Kind::Tts),
    (r"speecht5", Kind::Tts),
    // vision
    (r"florence", Kind::Vision),
    (r"\bllava\b", Kind::Vision),
    (r"\bclip\b", Kind::Vision),
    (r"\bsiglip\b", Kind::Vision),
    (r"\bblip\b", Kind::Vision),
    (r"\bqwen2\.5-vl\b", Kind::Vision),
    (r"\bqwen-vl\b", Kind::Vision),
    // embeddings
    (r"\bnomic[-_]?embed", Kind::Embed),
    (r"-bge-", Kind::Embed),
    (r"\bbge[-_]", Kind::Embed),
    (r"\bembed\b", Kind::Embed),
    (r"sentence[-_]?transformers", Kind::Embed),
    (r"e5[-_](small|base|large)", Kind::Embed),
    // wake-word — Slice 11.E. openWakeWord bundles live outside the HF
    // cache (under `wakeword_models_dir`), so these patterns mainly drive
    // kind overrides in service manifests + the future wake-word scanner.
    (r"\bopenwakeword\b", Kind::Wakeword),
    (r"\bwake[-_]?word\b", Kind::Wakeword),
];

static COMPILED: Lazy<Vec<(Regex, Kind)>> = Lazy::new(|| {
    PATTERNS
        .iter()
        .map(|(pat, kind)| {
            // case-insensitive — matches Python's re.IGNORECASE.
            let re = Regex::new(&format!("(?i){pat}")).expect("model_registry pattern compiles");
            (re, *kind)
        })
        .collect()
});

/// Heuristic kind inference from a model id or HF repo name. The
/// default — when nothing matches — is `Kind::Llm`. False `Llm` is
/// harmless (the inference bar shows it; the voice subsystem ignores
/// it). False STT/TTS would route a chat model into a subsystem that
/// can't run it.
pub fn infer_kind(repo_or_name: &str) -> Kind {
    if repo_or_name.is_empty() {
        return Kind::Llm;
    }
    for (re, kind) in COMPILED.iter() {
        if re.is_match(repo_or_name) {
            return *kind;
        }
    }
    Kind::Llm
}

/// Expose the (pattern, kind) list for tests and tooling.
pub fn iter_patterns() -> impl Iterator<Item = (&'static str, Kind)> {
    PATTERNS.iter().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_falls_back_to_llm() {
        assert_eq!(infer_kind(""), Kind::Llm);
    }

    #[test]
    fn whisper_is_stt() {
        assert_eq!(infer_kind("openai/whisper-small"), Kind::Stt);
        assert_eq!(infer_kind("distil-whisper/distil-large-v2"), Kind::Stt);
        assert_eq!(infer_kind("facebook/wav2vec2-base"), Kind::Stt);
    }

    #[test]
    fn piper_kokoro_are_tts() {
        assert_eq!(infer_kind("rhasspy/piper-voices"), Kind::Tts);
        assert_eq!(infer_kind("onnx-community/Kokoro-82M-v1.0-ONNX"), Kind::Tts);
        assert_eq!(infer_kind("microsoft/speecht5_tts"), Kind::Tts);
    }

    #[test]
    fn florence_is_vision() {
        assert_eq!(infer_kind("microsoft/Florence-2-large"), Kind::Vision);
        assert_eq!(infer_kind("Qwen/Qwen2.5-VL-7B-Instruct"), Kind::Vision);
        assert_eq!(infer_kind("openai/clip-vit-base-patch32"), Kind::Vision);
    }

    #[test]
    fn nomic_bge_are_embed() {
        assert_eq!(infer_kind("nomic-ai/nomic-embed-text-v1"), Kind::Embed);
        assert_eq!(infer_kind("BAAI/bge-small-en-v1.5"), Kind::Embed);
        assert_eq!(
            infer_kind("sentence-transformers/all-MiniLM-L6-v2"),
            Kind::Embed
        );
    }

    #[test]
    fn unknown_falls_back_to_llm() {
        assert_eq!(infer_kind("meta-llama/Llama-3.1-8B-Instruct"), Kind::Llm);
        assert_eq!(infer_kind("Qwen/Qwen2.5-14B-Instruct"), Kind::Llm);
        assert_eq!(infer_kind("totally-unknown-name"), Kind::Llm);
    }

    #[test]
    fn case_insensitive_matching() {
        assert_eq!(infer_kind("OpenAI/WHISPER-LARGE"), Kind::Stt);
        assert_eq!(infer_kind("MICROSOFT/FLORENCE-2-LARGE"), Kind::Vision);
    }

    #[test]
    fn openwakeword_is_wakeword() {
        assert_eq!(infer_kind("openWakeWord/hey-jarvis"), Kind::Wakeword,);
        assert_eq!(infer_kind("acme/my-wake-word-model"), Kind::Wakeword);
        assert_eq!(infer_kind("acme/wakeword-trained"), Kind::Wakeword);
    }

    #[test]
    fn iter_patterns_returns_full_set() {
        let collected: Vec<_> = iter_patterns().collect();
        assert_eq!(collected.len(), PATTERNS.len());
    }
}
