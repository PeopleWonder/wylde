//! Short model aliases for `/v1` (e.g. `coder` → a long HF GGUF id), so a
//! client config survives a model swap.
//!
//! Aliases live only in the harness model registry (`models.set_alias`,
//! #348). [`super::registry`] turns the registry's aliases into the
//! effective [`Aliases`] every route resolves with: the target is
//! installed, and a real model id beats an alias of the same name. If the
//! registry can't be reached, there are no aliases and names pass through
//! unchanged.

/// Effective alias → real model id pairs, as resolved by the registry view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aliases(Vec<(String, String)>);

impl Aliases {
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

    /// `(alias, target)` pairs in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(a, t)| (a.as_str(), t.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_maps_aliases_and_passes_other_names_through() {
        let a = Aliases::from_pairs(vec![("coder".into(), "real:1".into())]);
        assert_eq!(a.resolve("coder"), "real:1");
        assert_eq!(a.resolve("real:1"), "real:1");
        assert_eq!(a.resolve("other"), "other");
        assert_eq!(
            Aliases::default().resolve("x"),
            "x",
            "no aliases: unchanged"
        );
        assert_eq!(a.iter().collect::<Vec<_>>(), vec![("coder", "real:1")]);
    }
}
