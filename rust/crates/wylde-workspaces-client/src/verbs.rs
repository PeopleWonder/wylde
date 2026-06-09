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
    // ── Slice 0d — chat-turn prompt context ─────────────────────────────
    // Best-effort per-turn enrichment for the chat driver. Medium budget
    // (persona read + notes search [internally ≤1.2s] + a RAG embed) and
    // NoRetry: it's on the chat hot path, so a slow/unreachable service
    // must degrade to base context fast rather than stack retry budgets
    // onto every turn. Fail-fast → the driver's graceful-degrade notice.
    VerbDef {
        name: "workspaces.gather_prompt",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    // ── Slice B — code graph read API (Build Order Appendix A / Plan v2 §7) ─
    // "graph load" → Medium (§7.2); an idempotent read (§7.3, which lists
    // `graph` by name → exp-backoff ≤4); 5s cache TTL (§7.6 — the graph
    // changes on each ingest). NB: this follows the canonical Plan v2 §7 /
    // Appendix A policy (Medium · retry · 5s), NOT the Slice-B task brief's
    // "Slow · NoRetry" — see the slice report for the reconciliation.
    VerbDef {
        name: "workspaces.graph",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: Some(Duration::from_secs(5)),
    },
    // ── Slice 0c — workspace notes tier (Build Order Appendix A) ─────────
    VerbDef {
        name: "workspaces.notes.list",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.notes.add",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.notes.update",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.notes.delete",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.notes.search",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.notes.propose",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    // ── Slice 0c — workspace-scoped conversations ───────────────────────
    // Not in Appendix A under this name (it lists the `chat.*` workspace
    // surface for Slices E/J); the 0c lifecycle subset is assigned by shape
    // — Medium reads, non-idempotent delete — per the plan's "most note /
    // conversation verbs are Medium tier" guidance.
    VerbDef {
        name: "workspaces.conversations.list",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.conversations.get",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.conversations.delete",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
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
