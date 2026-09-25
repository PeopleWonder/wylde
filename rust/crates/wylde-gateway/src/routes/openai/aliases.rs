//! Short model aliases for `/v1` (e.g. `coder` → a long HF GGUF id), so a
//! client config survives a model swap.
//!
//! The source of truth is the harness model registry (`models.set_alias`,
//! #348). [`super::registry`] turns it into the effective [`Aliases`] every
//! route resolves with: target installed, and a real model id beats an
//! alias of the same name.
//!
//! **Deprecated stopgap:** `WYLDE_OPENAI_MODEL_ALIASES` (comma-separated
//! `alias=model_id` pairs, e.g.
//! `coder=hf.co/unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF:UD-IQ3_XXS,embed=nomic-embed-text:latest`)
//! still works during the migration, but only as a fallback: an env alias
//! applies where the registry doesn't define that name, or when the harness
//! is unreachable. Move entries into the registry with `models.set_alias`.
//! Malformed pairs are skipped and the first definition of an alias wins.

/// Env var holding the deprecated alias pairs.
pub const ALIASES_ENV: &str = "WYLDE_OPENAI_MODEL_ALIASES";

/// Alias → real model id, in definition order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aliases(Vec<(String, String)>);

impl Aliases {
    /// Parse `alias=model_id` pairs separated by commas.
    pub fn parse(spec: &str) -> Self {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for part in spec.split(',') {
            let Some((alias, target)) = part.split_once('=') else {
                continue;
            };
            let (alias, target) = (alias.trim(), target.trim());
            if alias.is_empty() || target.is_empty() || pairs.iter().any(|(a, _)| a == alias) {
                continue;
            }
            pairs.push((alias.to_owned(), target.to_owned()));
        }
        Self(pairs)
    }

    /// Read the deprecated [`ALIASES_ENV`] (empty when unset), warning when
    /// it is still in use.
    pub fn from_env() -> Self {
        let aliases = Self::parse(&std::env::var(ALIASES_ENV).unwrap_or_default());
        if !aliases.0.is_empty() {
            tracing::warn!(
                "{ALIASES_ENV} is deprecated: model aliases now live in the model \
                 registry (models.set_alias); the env entries are only a fallback"
            );
        }
        aliases
    }

    /// Aliases from already-resolved `(alias, target)` pairs.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    /// The real model id for `id`: its target when `id` is an alias,
    /// otherwise `id` unchanged.
    pub fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        self.0
            .iter()
            .find(|(a, _)| a == id)
            .map_or(id, |(_, t)| t.as_str())
    }

    /// `(alias, target)` pairs in definition order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(a, t)| (a.as_str(), t.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_and_skips_malformed() {
        let a = Aliases::parse(
            " coder = hf.co/u/Repo:Q3 , bad, =x, y=, embed=nomic:latest, coder=other",
        );
        assert_eq!(
            a.iter().collect::<Vec<_>>(),
            vec![("coder", "hf.co/u/Repo:Q3"), ("embed", "nomic:latest")]
        );
    }

    #[test]
    fn resolve_maps_aliases_and_passes_real_ids_through() {
        let a = Aliases::parse("coder=real:1");
        assert_eq!(a.resolve("coder"), "real:1");
        assert_eq!(a.resolve("real:1"), "real:1");
        assert_eq!(a.resolve("other"), "other");
        assert_eq!(Aliases::default().resolve("x"), "x");
    }
}
