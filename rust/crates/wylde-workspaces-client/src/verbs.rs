//! The verb definition table.
//!
//! One [`VerbDef`] per `workspaces.*` verb, pairing the verb name with its
//! timeout tier ([`crate::timeouts`]), retry policy ([`crate::retry`]), and
//! optional cache TTL ([`crate::cache`]) — the per-verb knobs from the
//! Build Order Appendix A. The client looks a verb up here at call time, so
//! adding a verb in a later slice is a one-line table entry, not new control
//! flow.
//!
//! Slice 0a defines only `ping`. The commented rows below are the shape
//! later slices fill in (kept as a map of the road ahead, not active code).

use std::time::Duration;

use crate::retry::RetryPolicy;
use crate::timeouts::TimeoutPolicy;

/// Per-verb client policy.
#[derive(Debug, Clone, Copy)]
pub struct VerbDef {
    /// Verb name as sent in the action envelope (`action` field).
    pub name: &'static str,
    /// Per-attempt timeout policy.
    pub timeout: TimeoutPolicy,
    /// Retry-on-transport-failure policy.
    pub retry: RetryPolicy,
    /// Read-through cache TTL, or `None` for no caching.
    pub cache_ttl: Option<Duration>,
}

/// The verb table. Looked up by [`lookup`].
///
/// Slice 0a: `ping` only — Fast tier, idempotent-read retry, no cache.
/// Later slices append their verbs here (see Build Order Appendix A for the
/// full tier/retry/TTL matrix):
///   - `workspaces.list_mru`   Fast · read · 30s   (0b)
///   - `workspaces.graph`      Medium · read · 5s  (B)
///   - `workspaces.symbols.find` Fast · read · 60s (F-data)
///   - `workspaces.symbol_context` PerHop · read · — (G-data)
///   - …
static TABLE: &[VerbDef] = &[VerbDef {
    name: "ping",
    timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
    retry: RetryPolicy::ExponentialBackoff {
        max_attempts: 3,
        initial_ms: 50,
    },
    cache_ttl: None,
}];

/// Look up the policy for `verb`, or `None` if the client doesn't know it.
pub fn lookup(verb: &str) -> Option<&'static VerbDef> {
    TABLE.iter().find(|v| v.name == verb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_is_defined() {
        let d = lookup("ping").expect("ping must be in the table");
        assert_eq!(d.name, "ping");
        assert_eq!(d.timeout, TimeoutPolicy::fast());
        assert!(d.cache_ttl.is_none());
        assert_eq!(d.retry.max_attempts(), 3);
    }

    #[test]
    fn unknown_verb_is_none() {
        assert!(lookup("workspaces.not_yet").is_none());
    }

    #[test]
    fn table_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for v in TABLE {
            assert!(seen.insert(v.name), "duplicate verb {:?}", v.name);
        }
    }
}
