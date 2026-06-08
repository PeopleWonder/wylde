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
/// Slice 0a: `ping`. Slice 0b: the relocated registry / persona / RAG verbs,
/// per Build Order Appendix A (tier · retry · TTL). The three verbs Appendix
/// A's 0b rows omit — `set_persona`, `rag_query`, `reindex` — are assigned by
/// their shape: `set_persona` is a small idempotent write (Fast); `rag_query`
/// embeds the query so it gets the Slow budget to clear the embedder's retry
/// window (it's fail-soft to empty, an idempotent read); `reindex` is the
/// long-running ingest kick (Slow · no-retry), matching Appendix A's
/// reindex/ingest note.
///
/// Later slices append their verbs here (graph / symbols.find / anchors / …).
static TABLE: &[VerbDef] = &[
    VerbDef {
        name: "ping",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::ExponentialBackoff {
            max_attempts: 3,
            initial_ms: 50,
        },
        cache_ttl: None,
    },
    // ── Slice 0b — registry / active-selection ──────────────────────────
    VerbDef {
        name: "workspaces.list_mru",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: Some(Duration::from_secs(30)),
    },
    VerbDef {
        name: "workspaces.set_active",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.create",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::SLOW),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.update",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.delete",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    // ── Slice 0b — persona ──────────────────────────────────────────────
    VerbDef {
        name: "workspaces.set_persona",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    // ── Slice 0b — RAG (PR #18 indexer) ─────────────────────────────────
    VerbDef {
        name: "workspaces.rag_query",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::SLOW),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.reindex",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::SLOW),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
];

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
