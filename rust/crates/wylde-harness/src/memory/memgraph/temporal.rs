//! Bi-temporal edge model for the memory graph — **P0**.
//!
//! Closes the file-structure / solidity-audit gap: today's typed
//! Entity→Entity edges are *relational* (an edge MERGE either exists or
//! it doesn't; [`super::cypher::upsert_edge`] mutates `r.weight` in
//! place), so **time is not first-class on the edges** — no event time,
//! no ingest time, no fact supersession, no point-in-time ("as-of")
//! reads.
//!
//! This module adds the **storage primitive** for two independent time
//! axes per edge, each a half-open interval `[from, to)` in epoch
//! milliseconds:
//!
//! * **Valid time** (`valid_from` / `valid_to`) — when the relationship
//!   was true *in the world*.
//! * **Transaction time** (`tx_from` / `tx_to`) — when *we* recorded /
//!   retired the row in our store.
//!
//! "Open-ended" is the sentinel [`OPEN`] (`i64::MAX`), not `NULL`, so
//! range predicates stay total and the property is always present.
//!
//! ## Supersession — invalidate, never overwrite
//!
//! Writing a relationship that already has a live edge does **not**
//! mutate it. It **closes the prior edge's `valid_to`** at the write
//! instant (preserving history) and **CREATE**s a fresh open edge. P0
//! closes *only* `valid_to`; `tx_to` stays [`OPEN`] so the superseded
//! edge remains *current belief* and is correctly returned by an
//! as-of read for `t` inside its (now-bounded) valid window. Closing
//! `tx_to` is a P1 concern (transaction-time *corrections*).
//!
//! ## Toggle — OFF ⇒ byte-identical
//!
//! [`temporal_memory_enabled`] reads `WYLDE_TEMPORAL_MEMORY`, **default
//! OFF**. With the toggle off the harness uses the existing relational
//! Cypher verbatim (same statements, same envelopes, same schema) — the
//! non-negotiable invariant, mirroring the concept-routing / hierarchy
//! gate convention. Nothing in this module runs on the OFF path; the
//! `bolt.rs` verbs branch into it only when the toggle is on.
//!
//! ## Why a pure in-memory reference model
//!
//! [`TemporalEdgeLog`] is an executable spec of the insert / supersede /
//! as-of semantics the Cypher mirrors. It lets the temporal edge model
//! be unit-tested **without a live Neo4j** (the crash-safety constraint
//! forbids bringing up the full stack), and doubles as documentation.
//! The live round-trip is covered by an `#[ignore]` integration test.

/// Open-ended interval sentinel. An edge with `valid_to == OPEN` is
/// still true; `tx_to == OPEN` means it is current belief. Chosen over
/// `NULL` so as-of range predicates (`$t < r.valid_to`) stay total and
/// the property is always present + indexable.
pub const OPEN: i64 = i64::MAX;

/// Toggle env var. `1` / `true` / `on` / `yes` (case-insensitive) ⇒ ON;
/// anything else, including unset, ⇒ OFF.
pub const TOGGLE_ENV: &str = "WYLDE_TEMPORAL_MEMORY";

/// Whether the bi-temporal edge path is enabled. **Default OFF** — the
/// OFF path is byte-identical to today's relational behavior. Read once
/// per call (cheap env read) so tests can flip it per-test without a
/// shared `OnceLock` reset, matching `memory::impl_for`'s convention.
pub fn temporal_memory_enabled() -> bool {
    match std::env::var(TOGGLE_ENV) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        ),
        Err(_) => false,
    }
}

/// Current wall-clock in epoch milliseconds. Used by the production
/// temporal write path; tests pass explicit timestamps for determinism.
/// A pre-epoch clock (impossible in practice) floors to 0.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Temporal Cypher builders ──────────────────────────────────────────
//
// `cypher.rs` (the verbatim Python port) is intentionally untouched;
// all temporal Cypher lives here so the additive, gated nature stays
// obvious. Each builder interpolates `rel_type` because Cypher forbids
// `$`-substitution in relationship-type position — the caller MUST
// validate `rel_type` via `super::schema::relation_type_is_valid`
// first, exactly as `cypher::relate_typed` requires.

/// Close the valid-time of the currently-open edge between two named
/// entities. The historical row is preserved, now bounded at `$at`.
/// `tx_to` is deliberately left untouched (stays [`OPEN`]) — see module
/// docs.
pub fn temporal_supersede(rel_type: &str) -> String {
    format!(
        "
MATCH (a:Entity {{name: $source}})-[r:{rel_type}]->(b:Entity {{name: $target}})
WHERE r.valid_to = $open
SET   r.valid_to = $at
"
    )
}

/// Create a fresh open edge (valid + tx intervals both `[$at, OPEN)`).
/// Endpoints are MERGE'd so the entities exist; the edge itself is
/// CREATE'd (not MERGE'd) so versioned history accumulates rather than
/// collapsing.
pub fn temporal_insert(rel_type: &str) -> String {
    format!(
        "
MERGE (a:Entity {{name: $source}})
MERGE (b:Entity {{name: $target}})
CREATE (a)-[r:{rel_type} {{valid_from: $at, valid_to: $open, tx_from: $at, tx_to: $open}}]->(b)
"
    )
}

/// Weighted variant of [`temporal_insert`] — carries `$weight` onto the
/// fresh edge so the weight *timeline* is reconstructable instead of a
/// single mutated scalar (the temporal answer to
/// [`super::cypher::upsert_edge`]).
pub fn temporal_insert_weighted(rel_type: &str) -> String {
    format!(
        "
MERGE (a:Entity {{name: $source}})
MERGE (b:Entity {{name: $target}})
CREATE (a)-[r:{rel_type} {{valid_from: $at, valid_to: $open, tx_from: $at, tx_to: $open, weight: $weight}}]->(b)
"
    )
}

/// Guarded relate — the live-path verb for plain (weightless) typed
/// edges. A weightless fact is either true or not, so re-relating an
/// already-open edge is a **no-op** (the `WHERE ex IS NULL` guard);
/// only a genuinely new relationship is CREATE'd. No supersession
/// happens here — weightless facts change only via retraction
/// (`unrelate`, P1). Mirrors [`TemporalEdgeLog::write`] with
/// `weight = None`.
pub fn temporal_relate_guarded(rel_type: &str) -> String {
    format!(
        "
MERGE (a:Entity {{name: $source}})
MERGE (b:Entity {{name: $target}})
WITH a, b
OPTIONAL MATCH (a)-[ex:{rel_type}]->(b) WHERE ex.valid_to = $open
WITH a, b, ex
WHERE ex IS NULL
CREATE (a)-[:{rel_type} {{valid_from: $at, valid_to: $open, tx_from: $at, tx_to: $open}}]->(b)
"
    )
}

/// Live-path verb for the **weighted** feedback edge — the temporal
/// answer to [`super::cypher::upsert_edge`]. Supersedes the open edge
/// (closes its `valid_to`) and CREATEs a fresh open edge whose `weight`
/// is the carried-forward cumulative (`coalesce(old.weight, 0) +
/// $weight_delta`), so the weight *timeline* is reconstructable. When no
/// open edge exists the `OPTIONAL MATCH` yields null, the `SET` no-ops,
/// and the new weight is just the delta — i.e. first-write create.
pub fn temporal_upsert_edge(rel_type: &str) -> String {
    format!(
        "
MERGE (a:Entity {{name: $source}})
MERGE (b:Entity {{name: $target}})
WITH a, b
OPTIONAL MATCH (a)-[old:{rel_type}]->(b) WHERE old.valid_to = $open
SET old.valid_to = $at
WITH a, b, coalesce(old.weight, 0) + $weight_delta AS new_weight
CREATE (a)-[:{rel_type} {{valid_from: $at, valid_to: $open, tx_from: $at, tx_to: $open, weight: new_weight}}]->(b)
"
    )
}

/// Point-in-time ("as-of") read of edges of one type that were **valid
/// at** event time `$t` under **current belief** (`r.tx_to = OPEN`).
/// Half-open valid interval: `valid_from <= $t < valid_to`.
pub fn as_of_match(rel_type: &str) -> String {
    format!(
        "
MATCH (a:Entity)-[r:{rel_type}]->(b:Entity)
WHERE r.valid_from <= $t AND $t < r.valid_to AND r.tx_to = $open
RETURN a.name      AS source,
       b.name      AS target,
       r.valid_from AS valid_from,
       r.valid_to   AS valid_to,
       r.tx_from    AS tx_from,
       r.tx_to      AS tx_to
"
    )
}

/// One-shot, idempotent migration of legacy (non-temporal) edges.
/// Backfills `valid_from` from any existing `r.created_at` (else the
/// configured `$epoch_default` floor) and opens `valid_to` / `tx_to`.
/// The `WHERE r.valid_from IS NULL` guard makes re-runs skip
/// already-migrated edges — safe to run every boot under the toggle.
/// The migration *run* is P1 (needs a live graph); this builder is the
/// P0 artifact.
pub fn backfill_temporal(rel_type: &str) -> String {
    format!(
        "
MATCH (a:Entity)-[r:{rel_type}]->(b:Entity)
WHERE r.valid_from IS NULL
SET   r.valid_from = coalesce(r.created_at, $epoch_default),
      r.valid_to   = $open,
      r.tx_from    = coalesce(r.created_at, $epoch_default),
      r.tx_to      = $open
"
    )
}

// ── In-memory reference model ─────────────────────────────────────────

/// One versioned edge. `weight` is `None` for plain typed edges and
/// `Some(_)` for the weighted feedback edge so the no-op guard can tell
/// an unchanged fact from a changed weight.
#[derive(Clone, Debug, PartialEq)]
pub struct TemporalEdge {
    pub source: String,
    pub target: String,
    pub rel_type: String,
    pub valid_from: i64,
    pub valid_to: i64,
    pub tx_from: i64,
    pub tx_to: i64,
    pub weight: Option<f64>,
}

impl TemporalEdge {
    /// Valid at event time `t` (half-open `[valid_from, valid_to)`).
    pub fn is_valid_at(&self, t: i64) -> bool {
        self.valid_from <= t && t < self.valid_to
    }

    /// Part of current belief (transaction interval still open).
    pub fn is_current_belief(&self) -> bool {
        self.tx_to == OPEN
    }

    /// The live valid row: open on both axes.
    pub fn is_open(&self) -> bool {
        self.valid_to == OPEN && self.tx_to == OPEN
    }
}

/// Append-only log of versioned edges — the executable spec the Cypher
/// mirrors. Insert / supersede / as-of are all pure, so the temporal
/// model is unit-testable with no Neo4j.
#[derive(Default, Clone, Debug)]
pub struct TemporalEdgeLog {
    edges: Vec<TemporalEdge>,
}

impl TemporalEdgeLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index of the currently-open edge (valid + current belief) for a
    /// triple, if one exists.
    fn open_index(&self, source: &str, target: &str, rel_type: &str) -> Option<usize> {
        self.edges.iter().position(|e| {
            e.source == source && e.target == target && e.rel_type == rel_type && e.is_open()
        })
    }

    /// Write a relationship at event time `at`.
    ///
    /// * If an identical open edge already exists (same triple **and**
    ///   same `weight`), this is a **no-op** — no zero-width churn.
    /// * Otherwise the prior open edge (if any) is **superseded** (its
    ///   `valid_to` closes at `at`; `tx_to` stays [`OPEN`]) and a fresh
    ///   open edge is appended.
    ///
    /// Returns `true` iff a new edge was written.
    pub fn write(
        &mut self,
        source: &str,
        target: &str,
        rel_type: &str,
        at: i64,
        weight: Option<f64>,
    ) -> bool {
        if let Some(i) = self.open_index(source, target, rel_type) {
            if self.edges[i].weight == weight {
                return false; // unchanged fact — no-op guard
            }
            self.edges[i].valid_to = at; // supersede: close valid-time only
        }
        self.edges.push(TemporalEdge {
            source: source.to_owned(),
            target: target.to_owned(),
            rel_type: rel_type.to_owned(),
            valid_from: at,
            valid_to: OPEN,
            tx_from: at,
            tx_to: OPEN,
            weight,
        });
        true
    }

    /// Logical delete: close the open edge's `valid_to` at `at` (the
    /// fact stopped being true) without inserting a replacement. Returns
    /// `true` iff a live edge was closed.
    pub fn retract(&mut self, source: &str, target: &str, rel_type: &str, at: i64) -> bool {
        match self.open_index(source, target, rel_type) {
            Some(i) => {
                self.edges[i].valid_to = at;
                true
            }
            None => false,
        }
    }

    /// As-of read: every edge **valid at** event time `t` under current
    /// belief. Excludes future edges, already-superseded windows, and
    /// retracted facts.
    pub fn as_of(&self, t: i64) -> Vec<&TemporalEdge> {
        self.edges
            .iter()
            .filter(|e| e.is_current_belief() && e.is_valid_at(t))
            .collect()
    }

    /// Full history (every version, superseded or live), insertion order.
    pub fn all(&self) -> &[TemporalEdge] {
        &self.edges
    }

    pub fn len(&self) -> usize {
        self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot+restore guard for the toggle env so tests don't leak
    /// into siblings. Mutual exclusion via the shared env lock.
    struct ToggleGuard {
        _g: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ToggleGuard {
        fn set(value: Option<&str>) -> Self {
            let g = crate::memory::common::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let prev = std::env::var(TOGGLE_ENV).ok();
            match value {
                Some(v) => std::env::set_var(TOGGLE_ENV, v),
                None => std::env::remove_var(TOGGLE_ENV),
            }
            Self { _g: g, prev }
        }
    }

    impl Drop for ToggleGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(TOGGLE_ENV, v),
                None => std::env::remove_var(TOGGLE_ENV),
            }
        }
    }

    // ── Toggle: default OFF, byte-identical invariant ──────────────────

    #[test]
    fn toggle_defaults_off_when_unset() {
        let _g = ToggleGuard::set(None);
        assert!(!temporal_memory_enabled());
    }

    #[test]
    fn toggle_on_for_truthy_values() {
        for v in ["1", "true", "TRUE", "on", "Yes", " on "] {
            let _g = ToggleGuard::set(Some(v));
            assert!(temporal_memory_enabled(), "{v:?} should enable");
        }
    }

    #[test]
    fn toggle_off_for_falsey_and_garbage() {
        for v in ["0", "false", "off", "no", "", "maybe", "2"] {
            let _g = ToggleGuard::set(Some(v));
            assert!(!temporal_memory_enabled(), "{v:?} should stay off");
        }
    }

    // ── Cypher builder pins ────────────────────────────────────────────

    #[test]
    fn supersede_closes_valid_to_only() {
        let q = temporal_supersede("CALLS");
        assert!(q.contains("[r:CALLS]"));
        assert!(q.contains("r.valid_to = $open"));
        assert!(q.contains("SET   r.valid_to = $at"));
        // P0 must NOT close tx_to on supersession (see module docs).
        assert!(!q.contains("tx_to ="), "P0 supersede must not touch tx_to");
    }

    #[test]
    fn insert_opens_both_axes() {
        let q = temporal_insert("IMPORTS");
        assert!(q.contains("CREATE (a)-[r:IMPORTS"));
        for needle in ["valid_from: $at", "valid_to: $open", "tx_from: $at", "tx_to: $open"] {
            assert!(q.contains(needle), "insert missing {needle}");
        }
        // MERGE endpoints, CREATE edge — history must accumulate.
        assert!(q.contains("MERGE (a:Entity"));
        assert!(q.contains("MERGE (b:Entity"));
        assert!(!q.contains("MERGE (a)-["), "edge must be CREATE'd, not MERGE'd");
    }

    #[test]
    fn weighted_insert_carries_weight() {
        let q = temporal_insert_weighted("MENTIONS");
        assert!(q.contains("weight: $weight"));
        assert!(q.contains("valid_to: $open"));
    }

    #[test]
    fn relate_guarded_creates_only_when_no_open_edge() {
        let q = temporal_relate_guarded("CALLS");
        assert!(q.contains("OPTIONAL MATCH (a)-[ex:CALLS]->(b)"));
        assert!(q.contains("ex.valid_to = $open"));
        assert!(q.contains("WHERE ex IS NULL"), "no-op guard for weightless edge");
        assert!(q.contains("CREATE (a)-[:CALLS"));
        // Weightless edge carries no weight property.
        assert!(!q.contains("weight"));
    }

    #[test]
    fn upsert_edge_supersedes_and_carries_cumulative_weight() {
        let q = temporal_upsert_edge("MENTIONS");
        assert!(q.contains("OPTIONAL MATCH (a)-[old:MENTIONS]->(b)"));
        assert!(q.contains("SET old.valid_to = $at"), "supersede prior weighted edge");
        assert!(q.contains("coalesce(old.weight, 0) + $weight_delta AS new_weight"));
        assert!(q.contains("weight: new_weight"));
    }

    #[test]
    fn as_of_uses_half_open_valid_window_under_current_belief() {
        let q = as_of_match("CONFIGURES");
        assert!(q.contains("[r:CONFIGURES]"));
        assert!(q.contains("r.valid_from <= $t"));
        assert!(q.contains("$t < r.valid_to"));
        assert!(q.contains("r.tx_to = $open"), "as-of reads current belief");
    }

    #[test]
    fn backfill_guards_on_null_valid_from_and_opens_intervals() {
        let q = backfill_temporal("EXPOSES");
        assert!(q.contains("WHERE r.valid_from IS NULL"), "idempotent guard");
        assert!(q.contains("coalesce(r.created_at, $epoch_default)"));
        assert!(q.contains("r.valid_to   = $open"));
        assert!(q.contains("r.tx_to      = $open"));
    }

    // ── Reference model: insert / supersede / as-of / no-op ────────────

    #[test]
    fn insert_creates_one_open_edge() {
        let mut log = TemporalEdgeLog::new();
        assert!(log.write("a", "b", "CALLS", 100, None));
        assert_eq!(log.len(), 1);
        let e = &log.all()[0];
        assert_eq!((e.valid_from, e.valid_to), (100, OPEN));
        assert_eq!((e.tx_from, e.tx_to), (100, OPEN));
        assert!(e.is_open());
    }

    #[test]
    fn supersede_bounds_prior_edge_and_preserves_history() {
        let mut log = TemporalEdgeLog::new();
        log.write("a", "b", "INHERITS", 100, None);
        // A *changed* fact (different weight) supersedes at t=200.
        assert!(log.write("a", "b", "INHERITS", 200, Some(1.0)));
        assert_eq!(log.len(), 2, "history grows, not collapses");

        let prior = &log.all()[0];
        assert_eq!(prior.valid_from, 100);
        assert_eq!(prior.valid_to, 200, "prior bounded at supersede instant");
        assert_eq!(prior.tx_to, OPEN, "tx_to stays open in P0 (still current belief)");

        let fresh = &log.all()[1];
        assert_eq!((fresh.valid_from, fresh.valid_to), (200, OPEN));
        assert!(fresh.is_open());
    }

    #[test]
    fn no_op_guard_skips_identical_open_edge() {
        let mut log = TemporalEdgeLog::new();
        assert!(log.write("a", "b", "CALLS", 100, None));
        // Re-writing the identical fact must not churn history.
        assert!(!log.write("a", "b", "CALLS", 150, None));
        assert!(!log.write("a", "b", "CALLS", 999, None));
        assert_eq!(log.len(), 1);
        assert_eq!(log.all()[0].valid_to, OPEN, "untouched");
    }

    #[test]
    fn weight_change_supersedes_but_same_weight_is_noop() {
        let mut log = TemporalEdgeLog::new();
        assert!(log.write("s", "t", "CALLS", 10, Some(1.0)));
        assert!(!log.write("s", "t", "CALLS", 20, Some(1.0)), "same weight = no-op");
        assert!(log.write("s", "t", "CALLS", 30, Some(2.5)), "new weight supersedes");
        assert_eq!(log.len(), 2);
        assert_eq!(log.all()[0].valid_to, 30);
        assert_eq!(log.all()[1].weight, Some(2.5));
    }

    #[test]
    fn as_of_returns_the_edge_live_at_t() {
        let mut log = TemporalEdgeLog::new();
        log.write("a", "b", "CALLS", 100, Some(1.0)); // valid [100, 200)
        log.write("a", "b", "CALLS", 200, Some(2.0)); // valid [200, OPEN)

        // Before anything existed.
        assert!(log.as_of(50).is_empty());
        // Inside the first window → first version (weight 1.0).
        let at150 = log.as_of(150);
        assert_eq!(at150.len(), 1);
        assert_eq!(at150[0].weight, Some(1.0));
        // At the supersede boundary (half-open) → the new version.
        let at200 = log.as_of(200);
        assert_eq!(at200.len(), 1);
        assert_eq!(at200[0].weight, Some(2.0));
        // Far future → still the open edge.
        assert_eq!(log.as_of(10_000)[0].weight, Some(2.0));
    }

    #[test]
    fn as_of_excludes_retracted_facts_after_retraction() {
        let mut log = TemporalEdgeLog::new();
        log.write("a", "b", "CALLS", 100, None);
        assert!(log.retract("a", "b", "CALLS", 300));
        // Still visible in its historical window...
        assert_eq!(log.as_of(150).len(), 1);
        // ...but gone at/after the retraction instant (half-open).
        assert!(log.as_of(300).is_empty());
        assert!(log.as_of(500).is_empty());
        // History is preserved, not deleted.
        assert_eq!(log.len(), 1);
        assert_eq!(log.all()[0].valid_to, 300);
    }

    #[test]
    fn retract_with_no_open_edge_is_false() {
        let mut log = TemporalEdgeLog::new();
        assert!(!log.retract("a", "b", "CALLS", 100));
        assert!(log.is_empty());
    }

    #[test]
    fn as_of_isolates_distinct_triples() {
        let mut log = TemporalEdgeLog::new();
        log.write("a", "b", "CALLS", 100, None);
        log.write("a", "b", "IMPORTS", 100, None); // different rel_type
        log.write("c", "d", "CALLS", 100, None); // different endpoints
        assert_eq!(log.as_of(150).len(), 3);
        // Superseding one triple doesn't disturb the others.
        log.retract("a", "b", "CALLS", 120);
        assert_eq!(log.as_of(150).len(), 2);
    }

    #[test]
    fn now_ms_is_positive_and_monotonic_enough() {
        let a = now_ms();
        let b = now_ms();
        assert!(a > 0);
        assert!(b >= a);
    }
}
