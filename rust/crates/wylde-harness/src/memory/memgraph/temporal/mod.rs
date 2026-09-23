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

/// **P1 — logical-delete `unrelate`.** Close the valid-time of the
/// currently-open edge between two named entities *without* inserting a
/// replacement: the fact **stopped being true** at `$at`. The historical
/// row is preserved (now bounded in valid-time) and, crucially, `tx_to`
/// is left [`OPEN`] so the retracted fact stays *current belief* and
/// remains visible to an as-of read of its (now-bounded) valid window —
/// the same reasoning that keeps supersession from closing `tx_to` (see
/// module docs §3.2). This is the temporal answer to
/// [`super::cypher::unrelate_typed`]'s hard `DELETE r`.
///
/// Mirrors [`TemporalEdgeLog::retract`]. Structurally identical to
/// [`temporal_supersede`] (both close only `valid_to` on the open row);
/// kept as a distinct verb because `unrelate` inserts nothing whereas
/// `relate`'s supersede is followed by a fresh [`temporal_insert`].
pub fn temporal_retract(rel_type: &str) -> String {
    format!(
        "
MATCH (a:Entity {{name: $source}})-[r:{rel_type}]->(b:Entity {{name: $target}})
WHERE r.valid_to = $open AND r.tx_to = $open
SET   r.valid_to = $at
"
    )
}

/// **P1 — transaction-time correction.** "We recorded the wrong value."
/// Retire our *belief* in the currently-open edge as of transaction time
/// `$tt` (close its `tx_to`, so it drops out of current belief) and
/// CREATE a corrected-belief edge that carries the **same real-world
/// valid window** (`valid_from` copied from the retired edge, `valid_to`
/// open) but a fresh transaction interval `[$tt, OPEN)` and the corrected
/// `$weight`. After this, an as-of read under current belief sees the
/// corrected weight, while an "as believed at `$tt - 1`" read still sees
/// the original — transaction-time travel (see [`as_believed_at_match`]).
///
/// Closing `tx_to` here (unlike supersession/retraction) is exactly the
/// P1 concern the module docs reserve the transaction axis for. Mirrors
/// [`TemporalEdgeLog::correct`].
pub fn temporal_correct(rel_type: &str) -> String {
    format!(
        "
MATCH (a:Entity {{name: $source}})-[old:{rel_type}]->(b:Entity {{name: $target}})
WHERE old.valid_to = $open AND old.tx_to = $open
WITH a, b, old, old.valid_from AS vf
SET old.tx_to = $tt
CREATE (a)-[:{rel_type} {{valid_from: vf, valid_to: $open, tx_from: $tt, tx_to: $open, weight: $weight}}]->(b)
"
    )
}

/// **P1 — dual-axis ("as believed at") read.** Every edge of one type
/// that was **valid at** event time `$t` **and believed at** transaction
/// time `$tt`. Both axes are half-open. With `$tt` = "now" this collapses
/// to the current-belief valid-time slice (equivalent to [`as_of_match`]
/// modulo the explicit `tx` range vs the `tx_to = OPEN` shortcut); with a
/// past `$tt` it reconstructs *what the store believed at that transaction
/// time* — corrections made after `$tt` are invisible.
pub fn as_believed_at_match(rel_type: &str) -> String {
    format!(
        "
MATCH (a:Entity)-[r:{rel_type}]->(b:Entity)
WHERE r.valid_from <= $t  AND $t  < r.valid_to
  AND r.tx_from   <= $tt AND $tt < r.tx_to
RETURN a.name      AS source,
       b.name      AS target,
       r.valid_from AS valid_from,
       r.valid_to   AS valid_to,
       r.tx_from    AS tx_from,
       r.tx_to      AS tx_to
"
    )
}

/// **P1 — gated edge-property index.** Neo4j 5.x / 2026.x relationship
/// RANGE index on one temporal property of one typed relation, backing
/// the as-of range scans (`valid_from`) and the transaction-axis slice
/// (`tx_from`). Idempotent (`IF NOT EXISTS`). Created only when the
/// toggle is ON so an OFF deployment's schema is byte-identical to today.
/// The index name is deterministic so `IF NOT EXISTS` de-dupes across
/// boots.
pub fn temporal_index(rel_type: &str, prop: &str) -> String {
    let name = format!("rel_{}_{}", rel_type.to_ascii_lowercase(), prop);
    format!("CREATE INDEX {name} IF NOT EXISTS FOR ()-[r:{rel_type}]-() ON (r.{prop})")
}

/// The five typed Entity→Entity relations that carry temporal edges
/// (mirrors [`super::schema::relation_type_is_valid`]). Iterated by the
/// gated schema/index setup and the migration backfill.
pub const TEMPORAL_RELATIONS: [&str; 5] = ["CALLS", "IMPORTS", "INHERITS", "CONFIGURES", "EXPOSES"];

/// The temporal edge properties given a RANGE index under the toggle.
/// `valid_from` backs as-of scans; `tx_from` backs the transaction-time
/// ("as believed at") slice.
pub const TEMPORAL_INDEXED_PROPS: [&str; 2] = ["valid_from", "tx_from"];

/// Backfill floor for legacy edges with no `created_at` — "dawn of
/// history" so migrated facts sort before any temporal write. `0`
/// (epoch) is safe: it is `< now_ms()` for every real write and `< OPEN`.
pub const EPOCH_DEFAULT_FLOOR: i64 = 0;

/// **P1 — temporal-aware `traverse`.** The as-of variant of
/// [`super::cypher::traverse_bucket`]: a graph walk whose typed-edge
/// expansion only follows edges that were **valid at** event time `$t`
/// under current belief (`tx_to = OPEN`). The temporal predicate is
/// applied via `all(rel IN relationships(tp) WHERE …)` over the typed
/// segment **only** — the trailing `MENTIONED_IN` hop is matched
/// separately so its (non-temporal) edges are *not* time-filtered
/// (applying the predicate to a property-less `MENTIONED_IN` edge would
/// null out every path and return zero chunks).
///
/// A zero-length typed segment (`*0..depth` matching the seed itself)
/// has an empty `relationships(tp)`, so `all(…)` is vacuously true and
/// seed-anchored chunks still surface — matching the relational bucket's
/// `*0..depth` behavior. `best_depth` is `length(tp)` directly (the
/// typed hop count), where the relational form used `length(p) - 1`
/// because its single path included the `MENTIONED_IN` hop.
///
/// Only reached when the toggle is ON **and** an `as_of` timestamp is
/// supplied; otherwise `traverse` runs the relational
/// [`super::cypher::traverse_bucket`] unchanged (OFF ⇒ byte-identical).
pub fn traverse_bucket_as_of(rel_types: &str, depth: u32, with_workspace: bool) -> String {
    let ws_filter = if with_workspace {
        " AND c.workspace = $ws "
    } else {
        ""
    };
    format!(
        "
UNWIND $names AS name
MATCH  (seed:Entity {{name: name}})
MATCH  tp = (seed)-[:{rel_types}*0..{depth}]-(e:Entity)
WHERE  all(rel IN relationships(tp) WHERE rel.valid_from <= $t AND $t < rel.valid_to AND rel.tx_to = $open)
MATCH  (e)-[:MENTIONED_IN]->(c:Chunk)
WHERE  1=1{ws_filter}
WITH   c, seed, length(tp) AS typed_depth
WITH   c, count(DISTINCT seed) AS seeds_touching, min(typed_depth) AS best_depth
RETURN c.id       AS id,
       c.path     AS path,
       c.symbol   AS symbol,
       c.language AS language,
       seeds_touching,
       best_depth
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

    /// **P1 — dual-axis read.** Every edge **valid at** event time `t`
    /// **and believed at** transaction time `tt` (both half-open). With
    /// `tt >= ` every edge's `tx_from` and no corrections yet, this
    /// equals [`Self::as_of`]; with a past `tt` it excludes edges whose
    /// belief began *after* `tt` (i.e. corrections made later), and
    /// re-includes edges whose belief was later retired but was still
    /// current at `tt`. Mirrors [`as_believed_at_match`].
    pub fn as_of_believed(&self, t: i64, tt: i64) -> Vec<&TemporalEdge> {
        self.edges
            .iter()
            .filter(|e| e.is_valid_at(t) && e.tx_from <= tt && tt < e.tx_to)
            .collect()
    }

    /// **P1 — transaction-time correction.** Retire our *belief* in the
    /// currently-open edge as of transaction time `tt` (close its
    /// `tx_to`) and append a corrected-belief edge carrying the **same
    /// valid window** (`valid_from` copied, `valid_to` open), a fresh
    /// transaction interval `[tt, OPEN)`, and `new_weight`. Returns
    /// `true` iff an open edge was corrected. Mirrors [`temporal_correct`].
    ///
    /// Distinct from [`Self::retract`]: retract closes *valid_to* (the
    /// fact stopped being true, still current belief); correct closes
    /// *tx_to* (we changed our mind about the recorded value).
    pub fn correct(
        &mut self,
        source: &str,
        target: &str,
        rel_type: &str,
        tt: i64,
        new_weight: Option<f64>,
    ) -> bool {
        match self.open_index(source, target, rel_type) {
            Some(i) => {
                let valid_from = self.edges[i].valid_from;
                self.edges[i].tx_to = tt; // retire belief in the wrong row
                self.edges.push(TemporalEdge {
                    source: source.to_owned(),
                    target: target.to_owned(),
                    rel_type: rel_type.to_owned(),
                    valid_from,
                    valid_to: OPEN,
                    tx_from: tt,
                    tx_to: OPEN,
                    weight: new_weight,
                });
                true
            }
            None => false,
        }
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
mod tests;
