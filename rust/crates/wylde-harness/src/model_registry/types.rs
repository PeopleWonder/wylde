//! Shared types for the unified model registry. Rust port of
//! `Core/harness/model_registry/_types.py`.
//!
//! `ModelEntry` is the lingua franca between the HF-cache scanner, the
//! service-manifest reader, the Ollama probe, and the LLM-routing
//! profiles. Every consumer (inference bar, voice, caption, embed
//! clients) sees the same shape regardless of where the model came
//! from.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The taxonomy bucket. Adding a kind here is a breaking change for
/// every consumer that filters on it — keep this list authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Chat / instruction-tuned LMs (qwen, llama, gemma, mistral, …)
    Llm,
    /// Speech-to-text (Whisper variants, distil-whisper, wav2vec2)
    Stt,
    /// Text-to-speech (Piper, Kokoro, XTTS, Bark, SpeechT5)
    Tts,
    /// Image / video understanding (Florence-2, LLaVA, CLIP, SigLIP)
    Vision,
    /// Embedding models (nomic-embed, BGE, sentence-transformers)
    Embed,
    /// Wake-word detection (openWakeWord bundles: melspectrogram +
    /// embedding + classifier ONNX trio). Added in Slice 11.E so
    /// `voice.check_wake_word_model` can filter by kind instead of
    /// piggy-backing on `Stt`. Models live under
    /// `%LOCALAPPDATA%\Wylde\voice\wakeword\<vendor>\<name>\` rather than
    /// the HF cache.
    Wakeword,
}

impl Kind {
    /// Lowercase wire string — matches the Python `Literal` values.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Llm => "llm",
            Kind::Stt => "stt",
            Kind::Tts => "tts",
            Kind::Vision => "vision",
            Kind::Embed => "embed",
            Kind::Wakeword => "wakeword",
        }
    }

    /// Parse a kind from its wire string, case-sensitive (matches Python
    /// behaviour). Returns `None` for unknown values. Convenience wrapper
    /// around the [`FromStr`] impl that hides the `Result` for callers
    /// that only care about Option semantics.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl FromStr for Kind {
    type Err = UnknownKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "llm" => Ok(Kind::Llm),
            "stt" => Ok(Kind::Stt),
            "tts" => Ok(Kind::Tts),
            "vision" => Ok(Kind::Vision),
            "embed" => Ok(Kind::Embed),
            "wakeword" => Ok(Kind::Wakeword),
            _ => Err(UnknownKind(s.to_owned())),
        }
    }
}

/// Error returned when parsing an unknown kind string. Carries the
/// rejected text for diagnostics; most callers just convert it to
/// `Option` via `.ok()`-equivalent helpers ([`Kind::parse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKind(pub String);

impl std::fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown model kind: {}", self.0)
    }
}

impl std::error::Error for UnknownKind {}

/// Every recognised kind, in the same order as Python's
/// `KIND_VALUES`. Used for diagnostics and "list all" queries.
/// Wakeword was added in Slice 11.E — append at the end (Python list
/// preserves order, so consumers iterating `KIND_VALUES` won't trip on
/// the new variant, only filter on it if they ask).
pub const KIND_VALUES: [Kind; 6] = [
    Kind::Llm,
    Kind::Stt,
    Kind::Tts,
    Kind::Vision,
    Kind::Embed,
    Kind::Wakeword,
];

/// One entry in the unified model registry. Cheap to construct — the
/// scanner builds one per HF cache directory on every refresh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Canonical id. HF repo (`microsoft/Florence-2-large`), Ollama tag
    /// (`qwen2.5:14b`), or manifest-declared name.
    pub id: String,
    pub kind: Kind,
    /// On-disk location, `None` for Ollama-only models.
    #[serde(default)]
    pub path: Option<String>,
    /// Disk footprint in bytes. 0 if unknown.
    #[serde(default)]
    pub size_bytes: u64,
    /// Currently resident in its inference engine.
    #[serde(default)]
    pub loaded: bool,
    /// `"huggingface"`, `"ollama"`, or `"local"`.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Service names that declared this model required.
    #[serde(default)]
    pub required_by: Vec<String>,
    /// Routing profile (only for LLM-kind models). Free-form JSON
    /// matching Python's schema.
    #[serde(default)]
    pub profile: Option<Value>,
    /// Last-accessed timestamp (epoch seconds).
    #[serde(default)]
    pub last_accessed: Option<f64>,
    /// Whether this entry should appear in the GUI's chat-model dropdown.
    #[serde(default)]
    pub chat_visible: bool,
}

fn default_provider() -> String {
    "huggingface".to_owned()
}

/// Default `chat_visible` for a kind — mirrors Python's
/// `default_chat_visible`. Only LLMs are visible by default; STT/TTS/
/// vision/embed are hidden so the inference bar doesn't offer Whisper
/// or Kokoro as something to "chat with".
pub fn default_chat_visible(kind: Kind) -> bool {
    matches!(kind, Kind::Llm)
}

impl ModelEntry {
    /// JSON-friendly view used by the inference bar and HTTP routes.
    /// Matches Python's `to_dict` output keys and ordering.
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "kind": self.kind.as_str(),
            "path": self.path,
            "size_bytes": self.size_bytes,
            "loaded": self.loaded,
            "provider": self.provider,
            "required_by": self.required_by,
            "profile": self.profile,
            "last_accessed": self.last_accessed,
            "chat_visible": self.chat_visible,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrips_via_string() {
        for k in KIND_VALUES {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn kind_unknown_string_is_none() {
        assert_eq!(Kind::parse("audio"), None);
        assert_eq!(Kind::parse(""), None);
        assert_eq!(Kind::parse("LLM"), None);
    }

    #[test]
    fn kind_fromstr_returns_unknown_for_bad_input() {
        let err: Result<Kind, _> = "audio".parse();
        let err = err.unwrap_err();
        assert_eq!(err.0, "audio");
        assert!(err.to_string().contains("audio"));
    }

    #[test]
    fn default_chat_visible_is_true_only_for_llm() {
        assert!(default_chat_visible(Kind::Llm));
        assert!(!default_chat_visible(Kind::Stt));
        assert!(!default_chat_visible(Kind::Tts));
        assert!(!default_chat_visible(Kind::Vision));
        assert!(!default_chat_visible(Kind::Embed));
        assert!(!default_chat_visible(Kind::Wakeword));
    }

    #[test]
    fn wakeword_kind_roundtrips() {
        assert_eq!(Kind::Wakeword.as_str(), "wakeword");
        assert_eq!(Kind::parse("wakeword"), Some(Kind::Wakeword));
        // KIND_VALUES must include the new variant so consumers that
        // iterate it (diagnostics, list-by-kind queries) see it.
        assert!(KIND_VALUES.contains(&Kind::Wakeword));
    }

    #[test]
    fn to_value_carries_every_field() {
        let entry = ModelEntry {
            id: "microsoft/Florence-2-large".into(),
            kind: Kind::Vision,
            path: Some("/some/path".into()),
            size_bytes: 1024,
            loaded: false,
            provider: "huggingface".into(),
            required_by: vec!["voice".into()],
            profile: None,
            last_accessed: Some(123.0),
            chat_visible: false,
        };
        let v = entry.to_value();
        assert_eq!(v["id"], "microsoft/Florence-2-large");
        assert_eq!(v["kind"], "vision");
        assert_eq!(v["path"], "/some/path");
        assert_eq!(v["size_bytes"], 1024);
        assert_eq!(v["provider"], "huggingface");
        assert_eq!(v["required_by"][0], "voice");
        assert_eq!(v["last_accessed"], 123.0);
        assert_eq!(v["chat_visible"], false);
    }
}
