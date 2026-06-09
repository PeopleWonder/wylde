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
    // ── Slice F-data — symbol index read API (Plan v2 §7 / Appendix A) ───
    // `symbols.find (cached)` → Fast · 500ms (§7.2 lists it under Fast); an
    // idempotent read (§7.3 → exp-backoff ≤4); 60s cache TTL (§7.6) — the
    // composer's per-keystroke highlighting (Slice F-visual) re-queries the
    // same name repeatedly, and symbols change only on ingest/watcher deltas,
    // so a 60s read-through cache (target <20ms, §2.5) is safe. NB: this
    // follows the canonical Plan v2 §7 / Appendix A policy (Fast · 60s), NOT
    // the F-data task brief's "Medium · 2s" — see the slice report for the
    // reconciliation (same brief-vs-spec call Slice B made for `graph`).
    VerbDef {
        name: "workspaces.symbols.find",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: Some(Duration::from_secs(60)),
    },
    // ── Slice G-data — symbol_context (Plan v2 §7 / Appendix A · OI-1) ────
    // Time-per-hop timeout `200ms + 300ms×N` (§7.2, the only PerHop verb);
    // the client resolves the budget from the request's `hops` at call time.
    // An idempotent read → exp-backoff ≤4 (§7.3 lists `symbol_context` by
    // name). NO cache (§7.6 / Appendix A leave its TTL blank — symbol context
    // must reflect the live graph after a file edit). NB: this follows the
    // canonical §7/Appendix-A policy, NOT the slice brief's "NoRetry · 30s
    // cache" — same spec-wins reconciliation Slice B applied; see the report.
    VerbDef {
        name: "workspaces.symbol_context",
        timeout: TimeoutPolicy::per_hop(),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
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
    // Slice E summary parity: the harness pushes a computed summary+embedding.
    // Fast (a small write — no graph/Ollama work service-side), NoRetry (a
    // non-idempotent fold; the next cadence re-summarises if it's lost), no
    // cache. Brief: Fast/NoRetry/write.
    VerbDef {
        name: "workspaces.conversations.refresh_summary",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    // ── Slice I — file watcher control (Fast lifecycle ops) ──────────────
    // status/pause/resume are cheap in-process control calls on the service.
    // Fast (500ms), a single retry (the spec's `retry: 1` — the loop won't
    // wedge on these), no cache (status must read live; pause/resume mutate).
    VerbDef {
        name: "workspaces.watcher.status",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.watcher.pause",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.watcher.resume",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    // ── Slice N-data — workspace anchor store (Build Order Appendix A) ────
    // Tiers/retry/cache taken verbatim from the canonical Plan v2 §7.2/§7.3/
    // §7.6 + Appendix A rows (NOT the slice brief, which had create/update/
    // delete as Fast·NoRetry and a find_by_token cache — same brief-vs-spec
    // reconciliation Slices B/F-data/G-data applied; see the slice report).
    // Only `anchors.list` is cached (§7.6 lists exactly `anchors.list` 30s).
    VerbDef {
        name: "workspaces.anchors.list",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: Some(Duration::from_secs(30)),
    },
    VerbDef {
        name: "workspaces.anchors.create",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.anchors.update",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_write(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.anchors.delete",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.anchors.find_by_token",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.anchors.find_by_target",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.anchors.list_under",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
        retry: RetryPolicy::idempotent_read(),
        cache_ttl: None,
    },
    VerbDef {
        name: "workspaces.anchors.propose",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::MEDIUM),
        retry: RetryPolicy::NoRetry,
        cache_ttl: None,
    },
    // ── Slice N-data-aliases — alias-driven promotion entry point ─────────
    // A small read+validate+audit call that returns the promotion payload, so
    // Fast (500ms). Promotion is non-idempotent and always user-confirmed
    // (Appendix A's promotion note: "no retry") → NoRetry. No cache (a write
    // intent; must reflect live state).
    VerbDef {
        name: "workspaces.anchors.promote_via_alias",
        timeout: TimeoutPolicy::Fixed(crate::timeouts::FAST),
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
    fn symbol_context_is_per_hop_idempotent_uncached() {
        let d = lookup("workspaces.symbol_context").expect("symbol_context defined");
        // Per-hop timeout (OI-1), resolving to the §2.5 budgets.
        assert_eq!(d.timeout, TimeoutPolicy::per_hop());
        assert_eq!(d.timeout.budget(1), Duration::from_millis(500));
        assert_eq!(d.timeout.budget(3), Duration::from_millis(1100));
        // Idempotent read → exp-backoff with >1 attempt; no read-through cache.
        assert!(d.retry.max_attempts() > 1, "idempotent read retries");
        assert!(d.cache_ttl.is_none(), "symbol_context is not cached (§7.6)");
    }

    #[test]
    fn anchor_verbs_match_appendix_a() {
        // Canonical Plan v2 §7 / Build Order Appendix A tiers.
        let list = lookup("workspaces.anchors.list").expect("list");
        assert_eq!(list.timeout, TimeoutPolicy::fast());
        assert_eq!(list.cache_ttl, Some(Duration::from_secs(30)));
        assert!(list.retry.max_attempts() > 1, "idempotent read");

        let create = lookup("workspaces.anchors.create").expect("create");
        assert_eq!(create.timeout.budget(1), Duration::from_secs(2)); // Medium
        assert_eq!(create.retry.max_attempts(), 2, "idempotent write = 1 retry");
        assert!(create.cache_ttl.is_none());

        let delete = lookup("workspaces.anchors.delete").expect("delete");
        assert_eq!(delete.retry.max_attempts(), 1, "non-idempotent = no retry");

        // find_by_token is Fast + uncached (§7.6 caches only anchors.list).
        let fbt = lookup("workspaces.anchors.find_by_token").expect("find_by_token");
        assert_eq!(fbt.timeout, TimeoutPolicy::fast());
        assert!(fbt.cache_ttl.is_none());

        let fbtg = lookup("workspaces.anchors.find_by_target").expect("find_by_target");
        assert_eq!(fbtg.timeout.budget(1), Duration::from_secs(2)); // Medium

        let under = lookup("workspaces.anchors.list_under").expect("list_under");
        assert_eq!(under.timeout, TimeoutPolicy::fast());

        let propose = lookup("workspaces.anchors.propose").expect("propose");
        assert_eq!(propose.retry.max_attempts(), 1, "non-idempotent");

        // Slice N-data-aliases: promote_via_alias is Fast · NoRetry · uncached.
        let promote = lookup("workspaces.anchors.promote_via_alias").expect("promote_via_alias");
        assert_eq!(promote.timeout, TimeoutPolicy::fast());
        assert_eq!(promote.retry.max_attempts(), 1, "non-idempotent promotion");
        assert!(promote.cache_ttl.is_none());
    }

    #[test]
    fn refresh_summary_is_fast_noretry_uncached() {
        // Slice E parity (Phase 2 polish): a small service-side write of a
        // harness-computed summary+embedding. Fast · NoRetry · no cache.
        let d = lookup("workspaces.conversations.refresh_summary")
            .expect("refresh_summary defined");
        assert_eq!(d.timeout, TimeoutPolicy::fast());
        assert_eq!(d.retry.max_attempts(), 1, "non-idempotent fold = no retry");
        assert!(d.cache_ttl.is_none());
    }

    #[test]
    fn table_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for v in TABLE {
            assert!(seen.insert(v.name), "duplicate verb {:?}", v.name);
        }
    }
}
