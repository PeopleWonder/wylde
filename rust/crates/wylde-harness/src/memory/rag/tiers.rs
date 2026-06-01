//! Tier vocabulary for the semantic memory layer.
//!
//! Mirrors `Core/harness/memory/rag.py::ALL_TIERS`. The tier values are
//! stored as plain strings in the JSON authoritative records so the wire
//! shape matches the Python implementation byte-for-byte and a future
//! parity test can compare envelopes verbatim.

pub const TIER_CORE: &str = "core";
pub const TIER_EPISODIC: &str = "episodic";
pub const TIER_SEMANTIC: &str = "semantic";
pub const TIER_PROCEDURAL: &str = "procedural";

pub const ALL_TIERS: &[&str] = &[TIER_CORE, TIER_EPISODIC, TIER_SEMANTIC, TIER_PROCEDURAL];

/// Strongly-typed tier handle. Useful for callers that want to refuse
/// unknown tier strings at compile time; everywhere else the raw `&str`
/// is fine and we match against `ALL_TIERS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    Core,
    Episodic,
    Semantic,
    Procedural,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Core => TIER_CORE,
            Tier::Episodic => TIER_EPISODIC,
            Tier::Semantic => TIER_SEMANTIC,
            Tier::Procedural => TIER_PROCEDURAL,
        }
    }
}

/// Parse a `&str` into a [`Tier`]. Returns `None` for unknown values —
/// callers can decide whether that's an error (`raise ValueError` in
/// Python's `rag.search`) or a silent "search all tiers" sentinel.
pub fn tier_from_str(s: &str) -> Option<Tier> {
    match s {
        TIER_CORE => Some(Tier::Core),
        TIER_EPISODIC => Some(Tier::Episodic),
        TIER_SEMANTIC => Some(Tier::Semantic),
        TIER_PROCEDURAL => Some(Tier::Procedural),
        _ => None,
    }
}

/// True if `s` is a recognised tier name.
pub fn is_known_tier(s: &str) -> bool {
    ALL_TIERS.contains(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tiers_match_python_string_constants() {
        assert_eq!(TIER_CORE, "core");
        assert_eq!(TIER_EPISODIC, "episodic");
        assert_eq!(TIER_SEMANTIC, "semantic");
        assert_eq!(TIER_PROCEDURAL, "procedural");
        assert_eq!(ALL_TIERS.len(), 4);
    }

    #[test]
    fn tier_from_str_known_values_round_trip() {
        for name in ALL_TIERS {
            let t = tier_from_str(name).unwrap();
            assert_eq!(t.as_str(), *name);
        }
    }

    #[test]
    fn tier_from_str_unknown_value_returns_none() {
        assert!(tier_from_str("workspace").is_none());
        assert!(tier_from_str("").is_none());
        assert!(tier_from_str("CORE").is_none(), "case-sensitive");
    }

    #[test]
    fn is_known_tier_matches_constants() {
        for name in ALL_TIERS {
            assert!(is_known_tier(name));
        }
        assert!(!is_known_tier("garbage"));
    }
}
