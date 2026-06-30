# Temporal (Bi-Temporal) Memory Graph — Scope & Design

Branch: `feat/temporal-memory-graph` (cut from `feat/thought-bubble-system` @ `0bcd1be`)
Status: **scope locked + P0 landed, HELD for review** (do not merge to trunk)
Author surface: `rust/crates/wylde-harness/src/memory/memgraph/` (storage / edge layer only)

---

## 1. Context & the gap

Today's memory is three layers (long-term / workspace / short-term) over a
Memgraph-backed (Neo4j 5.x Bolt) graph, plus importance/recency **scores on
nodes** and reflection cycles. Edges between graph nodes are *relational*:

* Typed Entity→Entity edges — `CALLS` / `IMPORTS` / `INHERITS` / `CONFIGURES`
  / `EXPOSES` (`cypher::relate_typed`, written via `BoltClient::relate` /
  embedded in `upsert`).
* `MENTIONED_IN` (Entity→Chunk).
* A weighted feedback edge upserted by `BoltClient::upsert_edge` (`r.weight`).

Node-level *supersession* already exists, but only on **long-term records**
(`long_term::LongTermMemory.superseded_by` — see `long_term/entries.rs`). That
is record history, not edge history.

**The gap** (per the file-structure / solidity audit): *time is not first-class
on the edges.* An edge MERGE either exists or it doesn't; `upsert_edge` mutates
`r.weight` in place. There is:

* no **event time** (when the relationship became true in the world),
* no **ingest / transaction time** (when *we* recorded it),
* no **fact supersession** (updating a relationship overwrites the prior state,
  destroying history), and
* no **point-in-time ("as-of") query** ("what did the graph believe at time *t*?").

This document specifies a **bi-temporal edge model** to close that gap, and the
**P0** slice that lands the storage primitive behind a toggle.

---

## 2. The bi-temporal edge model

Each temporal edge carries **two independent time axes**, each a half-open
interval `[from, to)` in **epoch milliseconds (i64)**:

| Axis | Property | Meaning |
|------|----------|---------|
| **Valid time** (event time) | `valid_from` | when the relationship became true *in the world* |
| | `valid_to` | when it stopped being true (open = still true) |
| **Transaction time** (ingest time) | `tx_from` | when this row was first recorded in *our* store |
| | `tx_to` | when this row was logically retired in our store (open = current belief) |

### 2.1 The open sentinel

"Open-ended" is represented by a **sentinel constant** rather than `NULL`:

```
OPEN = 9_223_372_036_854_775_807   (i64::MAX)
```

Rationale: range predicates stay total (`r.valid_to > $t` is always true for an
open edge, no `IS NULL` branch), the property is always present and indexable,
and migration of legacy edges yields a uniform shape. `NULL` was rejected
because every as-of predicate would need a `coalesce`/`IS NULL` arm, and mixed
NULL/value columns defeat Memgraph range scans.

### 2.2 Interval semantics

* Intervals are **half-open**: `from <= t < to`. An edge is *valid at t* iff
  `valid_from <= t AND t < valid_to`. It is *believed at t* (transaction axis)
  iff `tx_from <= t AND t < tx_to`.
* `from == to` ⇒ empty interval (an edge invalidated in the same instant it was
  inserted; legal but matches no `t`).
* A **current, true** edge has `valid_to == OPEN AND tx_to == OPEN`.

### 2.3 Why bi-temporal (not just valid-time)

Valid-time alone answers "when was this true?" Transaction-time additionally
answers "when did we *learn* it / *change our mind*?" and makes corrections
auditable: a wrong edge we later retract keeps its `tx` interval closed at the
correction instant, so a replay of "what the system believed last Tuesday" is
exact even after we fixed the data. This is the standard SQL:2011 bi-temporal
shape, mapped onto graph edges.

---

## 3. The supersession rule — invalidate, never overwrite

**On update of a relationship, INVALIDATE the prior edge; do not mutate it in
place.** Concretely, writing `(a)-[REL]->(b)` at time `t` when an open edge
already exists is a **two-statement** operation:

1. **Supersede** the currently-open edge(s):
   `SET r.valid_to = $t` on every `r` where `r.valid_to = OPEN` (i.e. the live
   valid row). The historical row is preserved verbatim, now bounded in
   valid-time. **`tx_to` is left OPEN** — see §3.2 for why.
2. **Insert** a fresh edge:
   `CREATE (a)-[:REL {valid_from:$t, valid_to:OPEN, tx_from:$t, tx_to:OPEN}]->(b)`.

Contrast with today's `relate_typed`, which is a single `MERGE (a)-[:REL]->(b)`
— idempotent, history-free. The temporal write **CREATE**s versioned edges so
the supersession chain accumulates rather than collapsing.

`unrelate` becomes a *logical* delete under the toggle: `SET r.valid_to = $t,
r.tx_to = $t` on the open row (the fact stopped being true at `t`), leaving the
history queryable. (Hard `DELETE` remains the toggle-OFF behavior; P0 leaves
`unrelate` on its existing path — see §8 deferred.)

`upsert_edge`'s weight bump becomes supersede-and-insert with the new weight
carried onto the fresh edge, so the weight *timeline* is reconstructable rather
than a single mutated scalar.

### 3.2 Why P0 supersession closes only `valid_to`, not `tx_to`

The whole point of the as-of read (§4) is **valid-time travel**: "what was true
at past time *t*?" If supersession closed `tx_to` on the prior edge, that edge
would no longer be *current belief*, and an as-of read under current belief
would **exclude** it — so `as_of(past_t)` for any relationship that has since
changed would return nothing. That defeats the feature.

So in P0, a write that supersedes closes **only `valid_to`**; the prior edge
keeps `tx_to = OPEN` and stays part of current belief, correctly returned by
`as_of(t)` for `t` in its (now-bounded) valid window. The transaction axis
(`tx_from`/`tx_to`) is fully present on every edge and reserved for P1
**corrections** ("we recorded the wrong value"), where closing `tx_to` is the
right move. P0 never closes `tx_to`; every edge it writes is current belief.

### 3.1 Idempotence / no-op guard

A temporal write that would reproduce the currently-open edge **unchanged**
(same endpoints, same payload) is a no-op: we do **not** supersede-and-reinsert
an identical fact, which would otherwise create zero-width churn and inflate
history. P0 keys "unchanged" on `(source, target, rel_type)` for `relate`
(typed edges are payload-free) and additionally on `weight` for `upsert_edge`.

---

## 4. The "as-of <t>" read API

A point-in-time read reconstructs the graph as it was **valid at** event time
`t` (and, optionally, **as believed at** transaction time `tt` — defaulting to
"now", i.e. current belief).

**Predicate** (the core fragment, reused by every as-of query):

```cypher
WHERE r.valid_from <= $t  AND $t  < r.valid_to     // valid-time slice
  AND r.tx_from   <= $tt AND $tt < r.tx_to          // transaction-time slice
```

With `$tt = OPEN-1` (or simply the open sentinel test `r.tx_to = OPEN`) the
transaction arm collapses to "current belief", giving the common
**valid-time-only as-of** read.

**Rust surface (P0):**

```rust
// On BoltClient — additive, gated, no change to existing reads when OFF.
async fn relations_as_of(&self, rel_type: &str, at_ms: i64) -> Reply
// → {"ok": true, "edges": [{source, target, valid_from, valid_to, tx_from, tx_to}, ...]}
```

Bi-temporal "as believed at" is the same query with an explicit `tt`; P0 ships
the valid-time slice with `tt = now` and leaves the dual-axis parameter on the
in-memory model (§7) so the Cypher can adopt it without an API break.

The default (no `at`) read path is **unchanged** — callers that don't ask for a
point in time get today's "latest" behavior.

---

## 5. Cypher / schema changes (additive, gated)

All temporal Cypher lives in a **new** module `memgraph/temporal.rs`;
`cypher.rs` (the verbatim Python port) is **untouched**. This keeps the gate
obvious and the diff additive.

### 5.1 New indexes (idempotent, added to `ensure_schema` only when ON)

```cypher
CREATE INDEX rel_valid_from IF NOT EXISTS FOR ()-[r:CALLS]-()      ON (r.valid_from)
-- ... per typed rel; plus a tx_from index. Memgraph/Neo4j edge-property index.
```

Edge-property indexes are only created under the toggle so an OFF deployment's
schema is byte-identical to today.

### 5.2 New statements (in `temporal.rs`)

* `temporal_supersede(rel_type)` — `MATCH (a {name:$source})-[r:REL]->(b {name:$target})
  WHERE r.valid_to = $open SET r.valid_to = $at` (closes valid-time only; `tx_to`
  stays OPEN — §3.2).
* `temporal_insert(rel_type)` — `MERGE (a:Entity {name:$source}) MERGE (b:Entity {name:$target})
  CREATE (a)-[r:REL {valid_from:$at, valid_to:$open, tx_from:$at, tx_to:$open}]->(b)`.
* `as_of_match(rel_type)` — the §4 predicate wrapped around a `MATCH`/`RETURN`.
* `backfill_temporal(rel_type)` — migration (§6).

Each is `rel_type`-interpolated (Cypher forbids `$`-substitution in
relationship-type position) and the caller validates `rel_type` against
`schema::relation_type_is_valid` exactly as `relate_typed` does today.

### 5.3 Toggle

`WYLDE_TEMPORAL_MEMORY` — read by `temporal::temporal_memory_enabled()`.
**Default OFF.** `1|true|on` ⇒ ON; anything else (incl. unset) ⇒ OFF. Mirrors
the concept-routing / hierarchy gate convention: **OFF ⇒ byte-identical to
today's relational behavior** (same Cypher, same envelopes, same schema). This
is the non-negotiable invariant and is unit-tested directly.

---

## 6. Migration path for existing edges

Existing edges have no temporal properties. Migration is a **one-shot,
idempotent backfill**, only meaningful once the toggle is ON:

```cypher
MATCH (a)-[r:REL]->(b)
WHERE r.valid_from IS NULL              // only un-migrated edges
SET   r.valid_from = coalesce(r.created_at, $epoch_default),
      r.valid_to   = $open,            // open-ended: still true
      r.tx_from    = coalesce(r.created_at, $epoch_default),
      r.tx_to      = $open
```

* **`valid_from` backfill** uses any existing edge timestamp (`r.created_at` if
  present from a prior write) else a configured `$epoch_default` (the index
  build time / "dawn of history" floor). The doc-recommended default is the
  earliest known graph-build timestamp so legacy facts sort before any new
  temporal write.
* **`valid_to` is left open-ended** (`OPEN`) — every legacy edge is assumed
  still true until something supersedes it.
* Idempotent: the `WHERE r.valid_from IS NULL` guard means re-running skips
  already-migrated edges. Safe to run on every boot under the toggle (like
  `ensure_schema`).
* **Reversible:** OFF deployments ignore the temporal properties entirely; the
  legacy `relate_typed` MERGE path reads/writes edges without them, so a
  migrated graph still serves the OFF path unchanged (extra properties are
  inert).

Migration is **specified here** and exposed as a builder in P0
(`temporal::backfill_temporal`), but the *run* (a live one-shot against the
production graph) is **deferred to P1** — it requires a live Memgraph and is
out of scope for the unit-tested storage primitive.

---

## 7. P0 scope (what lands now)

P0 = the storage/edge primitive, fully unit-tested DB-free, behind the toggle.

1. **`memgraph/temporal.rs`** (new): toggle reader, `OPEN` sentinel, `now_ms()`,
   the temporal Cypher builders (§5.2), the as-of predicate, and a **pure
   in-memory reference model** (`TemporalEdgeLog`) implementing
   insert / supersede / as-of semantics. The reference model is the executable
   spec the Cypher mirrors and the substrate for DB-free correctness tests.
2. **`bolt.rs` wiring**: `relate` and `upsert_edge` gain an early, gated branch
   — toggle ON routes to a temporal supersede-and-insert path; **toggle OFF
   leaves the existing body byte-for-byte unchanged** (same statements, same
   `{"ok":true,"written":N}` / `{"ok":true}` envelopes). A new
   `relations_as_of` read method (gated; the OFF answer is "temporal disabled").
3. **Tests** (all DB-free, following the existing mock/pure pattern):
   * insert → one open edge with correct `[valid_from, OPEN)`;
   * supersede → prior edge bounded at `t`, new open edge created, **history
     preserved** (count grows, old interval intact);
   * as-of → returns the edge live at `t`, excludes future/superseded edges;
   * no-op guard → re-writing an unchanged fact does not churn history;
   * **toggle-OFF identity** → with the toggle off, the Cypher produced for
     `relate` / `upsert_edge` is *identical* to `cypher::relate_typed` /
     `cypher::upsert_edge`, and `temporal_memory_enabled()` is `false` by
     default.
4. A live **`#[ignore]` integration test** (single Memgraph instance) exercising
   the real round-trip, following the `memgraph_integration.rs` pattern — not
   run under the crash-safety constraint, present for P1 verification.

### What P0 does **not** touch (integration-seam guardrails)

* **The reflection → memory write contract** and the **long-term memory API
  surface the harness calls** are *untouched*. This is the seam shared with the
  parallel agentic-tier work; the two branches must not collide. P0 stays
  strictly in the storage/edge layer (`memgraph/`).
* `long_term/`'s node-level `superseded_by` is left as-is (it is record history,
  orthogonal to edge history).
* `unrelate`'s hard-delete path, edge-property index creation against a live DB,
  dual-axis ("as believed at") Cypher, and the migration *run* are **P1**.

---

## 8. Deferred (P1+)

* Logical-delete `unrelate` (supersede instead of `DELETE r`) under the toggle.
* `ensure_schema` temporal edge-property indexes (gated) + live migration run.
* Dual-axis as-of Cypher (explicit transaction-time `tt` parameter).
* Temporal-aware `traverse` (as-of graph expansion, not just edge reads).
* Wiring the toggle into the GUI memory panel / surfacing edge history.

---

## 9. Testing & verification

* `cargo build` green, `cargo clippy` clean (capped jobs per crash-safety).
* DB-free unit tests for insert / supersede / as-of / no-op / toggle-OFF
  identity (the `TemporalEdgeLog` reference model + Cypher-string pins).
* Live round-trip behind `#[ignore]` (single Memgraph instance only).

The OFF-path identity test is the load-bearing one: it guarantees a deployment
that never sets `WYLDE_TEMPORAL_MEMORY` is indistinguishable from today.
