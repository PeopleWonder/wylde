//! Server-side fill-in-the-middle templates, keyed by model family.
//!
//! When a model doesn't report Ollama's `insert` capability, `/v1/completions`
//! renders the FIM prompt itself and sends it with `raw: true`. The
//! template's special tokens are always added as stop sequences, so the
//! model stops at the end of the gap instead of running on (measured: without
//! them Qwen3-Coder filled the gap, then wrote a whole extra function).

/// One family's FIM prompt layout and stop tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FimTemplate {
    pub family: &'static str,
    pub prefix: &'static str,
    pub suffix: &'static str,
    pub middle: &'static str,
    /// Suffix-first layout (Codestral: `[SUFFIX]…[PREFIX]…`).
    pub suffix_first: bool,
    pub stops: &'static [&'static str],
}

impl FimTemplate {
    /// The raw FIM prompt for the gap between `prefix` and `suffix`.
    pub fn render(&self, prefix: &str, suffix: &str) -> String {
        if self.suffix_first {
            format!(
                "{}{suffix}{}{prefix}{}",
                self.suffix, self.prefix, self.middle
            )
        } else {
            format!(
                "{}{prefix}{}{suffix}{}",
                self.prefix, self.suffix, self.middle
            )
        }
    }
}

const QWEN: FimTemplate = FimTemplate {
    family: "qwen-coder",
    prefix: "<|fim_prefix|>",
    suffix: "<|fim_suffix|>",
    middle: "<|fim_middle|>",
    suffix_first: false,
    stops: &[
        "<|endoftext|>",
        "<|fim_prefix|>",
        "<|fim_suffix|>",
        "<|fim_middle|>",
        "<|fim_pad|>",
        "<|repo_name|>",
        "<|file_sep|>",
        "<|im_end|>",
    ],
};

const CODESTRAL: FimTemplate = FimTemplate {
    family: "codestral",
    prefix: "[PREFIX]",
    suffix: "[SUFFIX]",
    middle: "",
    suffix_first: true,
    stops: &["[PREFIX]", "[SUFFIX]", "</s>"],
};

const STARCODER: FimTemplate = FimTemplate {
    family: "starcoder",
    prefix: "<fim_prefix>",
    suffix: "<fim_suffix>",
    middle: "<fim_middle>",
    suffix_first: false,
    stops: &[
        "<fim_prefix>",
        "<fim_suffix>",
        "<fim_middle>",
        "<|endoftext|>",
        "<file_sep>",
    ],
};

const DEEPSEEK: FimTemplate = FimTemplate {
    family: "deepseek-coder",
    prefix: "<｜fim▁begin｜>",
    suffix: "<｜fim▁hole｜>",
    middle: "<｜fim▁end｜>",
    suffix_first: false,
    stops: &[
        "<｜fim▁begin｜>",
        "<｜fim▁hole｜>",
        "<｜fim▁end｜>",
        "<|EOT|>",
    ],
};

/// The FIM template for a model id, by family name in the id
/// (case-insensitive). `None` when the family is unknown.
pub fn template_for(model: &str) -> Option<&'static FimTemplate> {
    let m = model.to_ascii_lowercase();
    if m.contains("codestral") {
        Some(&CODESTRAL)
    } else if m.contains("starcoder") {
        Some(&STARCODER)
    } else if m.contains("deepseek-coder") {
        Some(&DEEPSEEK)
    } else if m.contains("qwen") && m.contains("coder") {
        Some(&QWEN)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_templates_by_family() {
        let fam = |m: &str| template_for(m).map(|t| t.family);
        assert_eq!(
            fam("hf.co/unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF:UD-IQ3_XXS"),
            Some("qwen-coder")
        );
        assert_eq!(fam("qwen2.5-coder:3b-base"), Some("qwen-coder"));
        assert_eq!(fam("codestral:22b"), Some("codestral"));
        assert_eq!(fam("starcoder2:3b"), Some("starcoder"));
        assert_eq!(fam("deepseek-coder-v2:16b"), Some("deepseek-coder"));
        assert_eq!(
            fam("qwen3.5:9b"),
            None,
            "a non-coder Qwen has no FIM training"
        );
        assert_eq!(fam("gemma4:12b"), None);
    }

    #[test]
    fn renders_prefix_first_and_suffix_first_layouts() {
        assert_eq!(
            QWEN.render("def f():\n    ", "\n"),
            "<|fim_prefix|>def f():\n    <|fim_suffix|>\n<|fim_middle|>"
        );
        assert_eq!(CODESTRAL.render("A", "B"), "[SUFFIX]B[PREFIX]A");
    }

    #[test]
    fn every_template_stops_on_its_own_markers() {
        for t in [QWEN, CODESTRAL, STARCODER, DEEPSEEK] {
            for marker in [t.prefix, t.suffix, t.middle]
                .into_iter()
                .filter(|m| !m.is_empty())
            {
                assert!(
                    t.stops.contains(&marker),
                    "{} misses stop {marker}",
                    t.family
                );
            }
        }
    }
}
